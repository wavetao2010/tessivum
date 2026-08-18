use std::{
    env, fs,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use clap::error::ErrorKind;
use tessivum::{
    cli::{parse_cli, CliCommand, ExitClass, HeadlessCommand},
    headless::{run_headless, HeadlessConfig},
    host::{shutdown_signal, HostApi, HostConfig, HostRuntime},
    SessionId,
};

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
        CliCommand::Web => run_web().await,
        CliCommand::Sdk => run_sdk().await,
        // Keep plugin inspection isolated in the existing dedicated binary.
        CliCommand::PluginReport => Err(Diagnostic::runtime(
            "PLUGIN_REPORT_BINARY",
            "run the existing plugin_report binary for package inspection",
        )),
    }
}

async fn run_headless_command(command: HeadlessCommand) -> Result<(), Diagnostic> {
    let (config, task) = config(command).await?;
    let result = tokio::select! {
        result = run_headless(config, task) => result.map_err(|error| {
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

async fn run_web() -> Result<(), Diagnostic> {
    let frontend = web_frontend()?;
    let address = env::var("TESSIVUM_WEB_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3000".into())
        .parse::<SocketAddr>()
        .map_err(|error| Diagnostic::usage(format!("invalid TESSIVUM_WEB_ADDR: {error}")))?;
    let runtime = boot_host(env::var("TESSIVUM_REPLAY").ok()).await?;
    let host: Arc<dyn HostApi> = Arc::new(runtime.handle());
    let mut server = match tessivum::api::ApiServer::bind_with_config(
        host,
        tessivum::api::ApiServerConfig {
            bind_addr: address,
            frontend: Some(frontend),
        },
    )
    .await
    {
        Ok(server) => server,
        Err(error) => {
            let _ = runtime.shutdown().await;
            return Err(Diagnostic::runtime("WEB_BIND_FAILED", error));
        }
    };
    eprintln!("Tessivum web listening at http://{}", server.local_addr());
    let signal = match shutdown_signal().await {
        Ok(signal) => signal,
        Err(error) => {
            let _ = server.shutdown().await;
            let _ = runtime.shutdown().await;
            return Err(Diagnostic::runtime("SIGNAL_FAILED", error));
        }
    };
    let server_result = server
        .shutdown()
        .await
        .map_err(|error| Diagnostic::runtime("WEB_SHUTDOWN_FAILED", error));
    let host_result = runtime
        .shutdown()
        .await
        .map_err(|error| Diagnostic::runtime(error.code().to_owned(), error));
    server_result?;
    host_result?;
    if signal == 130 {
        Err(Diagnostic::cancelled("interrupted"))
    } else {
        Ok(())
    }
}

fn web_frontend() -> Result<tessivum::frontend::FrontendStatic, Diagnostic> {
    let web_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web");
    let dist = env::var_os("TESSIVUM_WEB_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| web_root.join("dist"));
    let frontend = tessivum::frontend::FrontendStatic::new(&dist)
        .map_err(|error| Diagnostic::runtime("WEB_FRONTEND_FAILED", error))?;
    let package_roots = match env::var_os("TESSIVUM_CLIENT_PACKAGES") {
        Some(paths) => env::split_paths(&paths).collect(),
        None => bundled_client_package_roots(&web_root)?,
    };
    frontend
        .scan_packages(package_roots)
        .map_err(|error| Diagnostic::runtime("WEB_CLIENT_PACKAGES_FAILED", error))?;
    Ok(frontend)
}

fn bundled_client_package_roots(web_root: &Path) -> Result<Vec<PathBuf>, Diagnostic> {
    let manifest_path = web_root.join("package.json");
    let manifest = fs::read(&manifest_path).map_err(|error| {
        Diagnostic::runtime(
            "WEB_PACKAGE_MANIFEST_FAILED",
            format!("{}: {error}", manifest_path.display()),
        )
    })?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest).map_err(|error| {
        Diagnostic::runtime(
            "WEB_PACKAGE_MANIFEST_FAILED",
            format!("{}: {error}", manifest_path.display()),
        )
    })?;
    let dependencies = match manifest.get("dependencies") {
        Some(value) => value.as_object().ok_or_else(|| {
            Diagnostic::runtime(
                "WEB_PACKAGE_MANIFEST_FAILED",
                format!(
                    "{}: dependencies must be an object",
                    manifest_path.display()
                ),
            )
        })?,
        None => return Ok(Vec::new()),
    };
    let node_modules = web_root.join("node_modules");
    let mut roots = Vec::new();
    for package in dependencies.keys() {
        let root = npm_package_root(&node_modules, package).ok_or_else(|| {
            Diagnostic::runtime(
                "WEB_PACKAGE_MANIFEST_FAILED",
                format!(
                    "{}: invalid dependency name {package:?}",
                    manifest_path.display()
                ),
            )
        })?;
        let package_manifest = root.join("package.json");
        let package = fs::read(&package_manifest).map_err(|error| {
            Diagnostic::runtime(
                "WEB_CLIENT_PACKAGE_MISSING",
                format!("{}: {error}", package_manifest.display()),
            )
        })?;
        let package: serde_json::Value = serde_json::from_slice(&package).map_err(|error| {
            Diagnostic::runtime(
                "WEB_CLIENT_PACKAGE_INVALID",
                format!("{}: {error}", package_manifest.display()),
            )
        })?;
        if package
            .get("dsh")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|dsh| dsh.contains_key("client"))
        {
            roots.push(root);
        }
    }
    Ok(roots)
}

fn npm_package_root(node_modules: &Path, package: &str) -> Option<PathBuf> {
    let mut components = Path::new(package).components();
    let first = match components.next()? {
        Component::Normal(component) => component,
        _ => return None,
    };
    match components.next() {
        None if !first.to_string_lossy().starts_with('@') => Some(node_modules.join(first)),
        Some(Component::Normal(second))
            if first.to_string_lossy().starts_with('@')
                && first.len() > 1
                && components.next().is_none() =>
        {
            Some(node_modules.join(first).join(second))
        }
        _ => None,
    }
}

enum SdkOutcome {
    Eof,
    Signal(i32),
}

async fn run_sdk() -> Result<(), Diagnostic> {
    let runtime = boot_host(None).await?;
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
            let _ = runtime.shutdown().await;
            return Err(error);
        }
    };
    runtime
        .shutdown()
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

async fn boot_host(recorded_replay: Option<String>) -> Result<HostRuntime, Diagnostic> {
    let cwd =
        env::current_dir().map_err(|error| Diagnostic::runtime("CWD_RESOLUTION_FAILED", error))?;
    let config = HostConfig::new(cwd.clone(), cwd.join(".tessivum"));
    HostRuntime::boot(match recorded_replay {
        Some(replay) => config.with_recorded_replay(replay),
        None => config,
    })
    .await
    .map_err(|error| Diagnostic::runtime(error.code().to_owned(), error))
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

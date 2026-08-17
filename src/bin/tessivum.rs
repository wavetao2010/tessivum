use std::{env, net::SocketAddr, process::ExitCode, sync::Arc};

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
            "run the existing tessivum-plugin-report binary for package inspection",
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
    let runtime = boot_host().await?;
    let host: Arc<dyn HostApi> = Arc::new(runtime.handle());
    let address = env::var("TESSIVUM_WEB_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3000".into())
        .parse::<SocketAddr>()
        .map_err(|error| Diagnostic::usage(format!("invalid TESSIVUM_WEB_ADDR: {error}")))?;
    let mut server = tessivum::api::ApiServer::bind_at(host, address)
        .await
        .map_err(|error| Diagnostic::runtime("WEB_BIND_FAILED", error))?;
    let signal = shutdown_signal()
        .await
        .map_err(|error| Diagnostic::runtime("SIGNAL_FAILED", error))?;
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

enum SdkOutcome {
    Eof,
    Signal(i32),
}

async fn run_sdk() -> Result<(), Diagnostic> {
    let runtime = boot_host().await?;
    let server = tessivum::sdk::JsonRpcServer::new(Arc::new(runtime.handle()));
    let reader = tokio::fs::File::open("/dev/stdin")
        .await
        .map_err(|error| Diagnostic::runtime("SDK_STDIN_FAILED", error))?;
    let writer = tokio::fs::OpenOptions::new()
        .write(true)
        .open("/dev/stdout")
        .await
        .map_err(|error| Diagnostic::runtime("SDK_STDOUT_FAILED", error))?;
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

async fn boot_host() -> Result<HostRuntime, Diagnostic> {
    let cwd =
        env::current_dir().map_err(|error| Diagnostic::runtime("CWD_RESOLUTION_FAILED", error))?;
    HostRuntime::boot(HostConfig::new(cwd.clone(), cwd.join(".tessivum")))
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

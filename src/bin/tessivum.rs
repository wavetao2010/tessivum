use std::{env, process::ExitCode};

use clap::error::ErrorKind;
use tessivum::{
    cli::{parse_cli, CliCommand, ExitClass, HeadlessCommand},
    headless::{run_headless, HeadlessConfig},
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

    let CliCommand::Headless(command) = command else {
        let name = match command {
            CliCommand::Sdk => "sdk",
            CliCommand::Web => "web",
            CliCommand::PluginReport => "plugin-report",
            CliCommand::Headless(_) => unreachable!("headless was matched above"),
        };
        return Err(Diagnostic::runtime(
            "NOT_YET_ACTIVE",
            format!("{name} is not yet active"),
        ));
    };

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

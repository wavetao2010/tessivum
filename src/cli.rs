use std::{ffi::OsString, num::NonZeroU64, path::PathBuf};

use clap::{error::ErrorKind, Args, Error, Parser, Subcommand, ValueEnum};

/// A fully parsed launcher invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cli {
    pub command: CliCommand,
}

/// The command selected by the launcher.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CliCommand {
    Headless(HeadlessCommand),
    Sdk,
    Web(WebCommand),
    PluginReport,
}

/// Inputs owned by the web launcher profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebCommand {
    pub patches: Vec<PathBuf>,
}

/// Inputs owned by the headless launcher, independent of service internals.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeadlessCommand {
    pub patches: Vec<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub session: Option<String>,
    pub resume: bool,
    pub replay: Option<PathBuf>,
    pub provider: String,
    pub model: String,
    pub max_tokens: Option<NonZeroU64>,
    pub trusted_bash: bool,
    pub task: String,
}

/// Process outcomes with stable shell-facing exit codes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitClass {
    Usage,
    Runtime,
    Cancelled,
}

impl ExitClass {
    pub const fn code(self) -> i32 {
        match self {
            Self::Usage => 2,
            Self::Runtime => 1,
            Self::Cancelled => 130,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Profile {
    Headless,
    Web,
}

#[derive(Debug, Parser)]
#[command(name = "tessivum", version, disable_help_subcommand = true)]
struct RawCli {
    #[arg(long, value_enum)]
    profile: Option<Profile>,
    #[arg(long = "patch", value_name = "PATCH", global = true)]
    patches: Vec<PathBuf>,
    #[command(subcommand)]
    command: Option<RawCommand>,
    #[command(flatten)]
    headless: RawHeadlessCommand,
}

#[derive(Debug, Subcommand)]
enum RawCommand {
    Sdk,
    Web,
    #[command(name = "plugin-report")]
    PluginReport,
}

#[derive(Debug, Args)]
struct RawHeadlessCommand {
    #[arg(long, value_name = "DIR")]
    data_dir: Option<PathBuf>,
    #[arg(long, value_name = "SESSION")]
    session: Option<String>,
    #[arg(long)]
    resume: bool,
    #[arg(long, value_name = "FILE")]
    replay: Option<PathBuf>,
    #[arg(long, value_name = "PROVIDER")]
    provider: Option<String>,
    #[arg(long, value_name = "MODEL")]
    model: Option<String>,
    #[arg(long, value_name = "TOKENS")]
    max_tokens: Option<NonZeroU64>,
    #[arg(long)]
    trusted_bash: bool,
    #[arg(value_name = "TASK", num_args = 0..)]
    task: Vec<String>,
}

/// Parses explicit process arguments without printing, exiting, or reading process state.
pub fn parse_cli<I, T>(args: I) -> Result<Cli, Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let raw = RawCli::try_parse_from(args.clone())?;

    if let Some(command) = raw.command {
        if raw.profile.is_some() || raw.headless.is_set() {
            return Err(usage_error(
                "commands sdk, web, and plugin-report do not accept headless launcher options",
            ));
        }
        let command = match command {
            RawCommand::Web => CliCommand::Web(WebCommand {
                patches: raw.patches,
            }),
            RawCommand::Sdk => {
                if !raw.patches.is_empty() {
                    return Err(usage_error("the sdk command does not accept --patch"));
                }
                CliCommand::Sdk
            }
            RawCommand::PluginReport => {
                if !raw.patches.is_empty() {
                    return Err(usage_error(
                        "the plugin-report command does not accept --patch",
                    ));
                }
                CliCommand::PluginReport
            }
        };
        return Ok(Cli { command });
    }

    match raw.profile.unwrap_or(Profile::Headless) {
        Profile::Web => {
            if raw.headless.is_set() {
                return Err(usage_error(
                    "the web profile does not accept headless launcher options",
                ));
            }
            Ok(Cli {
                command: CliCommand::Web(WebCommand {
                    patches: raw.patches,
                }),
            })
        }
        Profile::Headless => {
            reject_launcher_options_after_task(&args)?;
            headless_command(raw.headless, raw.patches).map(|command| Cli {
                command: CliCommand::Headless(command),
            })
        }
    }
}

impl RawHeadlessCommand {
    fn is_set(&self) -> bool {
        self.data_dir.is_some()
            || self.session.is_some()
            || self.resume
            || self.replay.is_some()
            || self.provider.is_some()
            || self.model.is_some()
            || self.max_tokens.is_some()
            || self.trusted_bash
            || !self.task.is_empty()
    }
}

fn headless_command(
    raw: RawHeadlessCommand,
    patches: Vec<PathBuf>,
) -> Result<HeadlessCommand, Error> {
    if raw.resume && raw.session.is_none() {
        return Err(usage_error("--resume requires --session"));
    }
    let provider = raw.provider.clone().unwrap_or_else(|| "recorded".into());
    if provider == "recorded" && raw.replay.is_none() {
        return Err(usage_error("--provider recorded requires --replay <FILE>"));
    }
    if raw.replay.is_none() && raw.model.is_none() {
        return Err(usage_error("a live --provider requires --model <MODEL>"));
    }

    let task = raw.task.join(" ");
    if task.trim().is_empty() {
        return Err(usage_error(
            "a non-blank task is required for the headless profile",
        ));
    }

    Ok(HeadlessCommand {
        patches,
        data_dir: raw.data_dir,
        session: raw.session,
        resume: raw.resume,
        replay: raw.replay,
        provider,
        model: raw.model.unwrap_or_else(|| "recorded".into()),
        max_tokens: raw.max_tokens,
        trusted_bash: raw.trusted_bash,
        task,
    })
}

fn reject_launcher_options_after_task(args: &[OsString]) -> Result<(), Error> {
    let mut cursor = 1;
    let mut task_started = false;

    while let Some(argument) = args.get(cursor).and_then(|argument| argument.to_str()) {
        if task_started {
            if is_launcher_option(argument) {
                return Err(usage_error("launcher options must appear before the task"));
            }
            cursor += 1;
            continue;
        }

        if argument == "--" {
            task_started = true;
            cursor += 1;
        } else if argument == "--resume" {
            cursor += 1;
        } else if launcher_option_with_value(argument).is_some() {
            cursor += usize::from(!argument.contains('='));
            cursor += 1;
        } else if argument.starts_with('-') {
            cursor += 1;
        } else {
            task_started = true;
            cursor += 1;
        }
    }

    Ok(())
}

fn is_launcher_option(argument: &str) -> bool {
    argument == "--resume"
        || argument == "--trusted-bash"
        || launcher_option_with_value(argument).is_some()
}

fn launcher_option_with_value(argument: &str) -> Option<&str> {
    [
        "--profile",
        "--patch",
        "--data-dir",
        "--session",
        "--replay",
        "--provider",
        "--model",
        "--max-tokens",
    ]
    .into_iter()
    .find(|option| {
        argument == *option
            || argument
                .strip_prefix(option)
                .is_some_and(|suffix| suffix.starts_with('='))
    })
}

fn usage_error(message: impl Into<String>) -> Error {
    Error::raw(ErrorKind::InvalidValue, message.into())
}

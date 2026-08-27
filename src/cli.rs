use std::{env, ffi::OsString, fmt, io, num::NonZeroU64, path::PathBuf};

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
    Sdk(SdkCommand),
    Web(WebCommand),
    Plugin(PluginCommand),
    PluginReport,
}

/// Inputs owned by the web launcher profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebCommand {
    pub data_dir: Option<PathBuf>,
    pub patches: Vec<PathBuf>,
}

/// Inputs owned by the SDK launcher profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SdkCommand {
    pub data_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginCommand {
    pub data_dir: Option<PathBuf>,
    pub action: PluginAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginAction {
    Add(String),
    Remove(String),
}

/// Inputs owned by the headless launcher, independent of service internals.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeadlessCommand {
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

/// The resolved process working directory and persistent data root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataRoot {
    pub cwd: PathBuf,
    pub data_dir: PathBuf,
}

/// Failures while selecting the persistent data root.
#[derive(Debug)]
pub enum DataRootError {
    CurrentDir(io::Error),
    RelativeTessivumHome(PathBuf),
    MissingHome,
    RelativeHome(PathBuf),
    LegacyCwd { legacy: PathBuf, target: PathBuf },
}

impl fmt::Display for DataRootError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDir(error) => write!(formatter, "cannot resolve current directory: {error}"),
            Self::RelativeTessivumHome(path) => write!(
                formatter,
                "TESSIVUM_HOME must be an absolute directory, got {}",
                path.display()
            ),
            Self::MissingHome => write!(
                formatter,
                "HOME is not set; set HOME or TESSIVUM_HOME, or pass --data-dir <DIR>"
            ),
            Self::RelativeHome(path) => write!(
                formatter,
                "HOME must be an absolute directory, got {}",
                path.display()
            ),
            Self::LegacyCwd { legacy, target } => write!(
                formatter,
                "default data root {} does not exist but legacy project data root {} exists; use it with --data-dir {} or move it to {}; no files were copied",
                target.display(),
                legacy.display(),
                legacy.display(),
                target.display()
            ),
        }
    }
}

impl std::error::Error for DataRootError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CurrentDir(error) => Some(error),
            Self::RelativeTessivumHome(_)
            | Self::MissingHome
            | Self::RelativeHome(_)
            | Self::LegacyCwd { .. } => None,
        }
    }
}

/// Resolves persistent storage without copying legacy project-local state.
pub fn resolve_data_root(data_dir: Option<PathBuf>) -> Result<DataRoot, DataRootError> {
    let cwd = env::current_dir().map_err(DataRootError::CurrentDir)?;
    if let Some(data_dir) = data_dir {
        return Ok(DataRoot {
            data_dir: if data_dir.is_absolute() {
                data_dir
            } else {
                cwd.join(data_dir)
            },
            cwd,
        });
    }

    if let Some(data_dir) = env::var_os("TESSIVUM_HOME").map(PathBuf::from) {
        if !data_dir.is_absolute() {
            return Err(DataRootError::RelativeTessivumHome(data_dir));
        }
        return Ok(DataRoot { cwd, data_dir });
    }

    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(DataRootError::MissingHome)?;
    if !home.is_absolute() {
        return Err(DataRootError::RelativeHome(home));
    }
    let data_dir = home.join(".tessivum");
    let legacy = cwd.join(".tessivum");
    if !data_dir.exists() && legacy.exists() {
        return Err(DataRootError::LegacyCwd {
            legacy,
            target: data_dir,
        });
    }
    Ok(DataRoot { cwd, data_dir })
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
    #[arg(long, value_name = "DIR", global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<RawCommand>,
    #[command(flatten)]
    headless: RawHeadlessCommand,
}

#[derive(Debug, Subcommand)]
enum RawCommand {
    Sdk,
    Web(RawWebCommand),
    Plugin(RawPluginCommand),
    #[command(name = "plugin-report")]
    PluginReport,
}

#[derive(Debug, Args)]
struct RawWebCommand {
    #[arg(long = "patch", value_name = "FILE")]
    patches: Vec<PathBuf>,
}

#[derive(Debug, Args)]
struct RawPluginCommand {
    #[command(subcommand)]
    action: RawPluginAction,
}

#[derive(Debug, Subcommand)]
enum RawPluginAction {
    Add { specifier: String },
    Remove { package: String },
}

#[derive(Debug, Args)]
struct RawHeadlessCommand {
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
                "commands sdk, web, plugin, and plugin-report do not accept headless launcher options",
            ));
        }
        let command = match command {
            RawCommand::Web(web) => CliCommand::Web(WebCommand {
                data_dir: raw.data_dir,
                patches: web.patches,
            }),
            RawCommand::Sdk => CliCommand::Sdk(SdkCommand {
                data_dir: raw.data_dir,
            }),
            RawCommand::Plugin(plugin) => CliCommand::Plugin(PluginCommand {
                data_dir: raw.data_dir,
                action: match plugin.action {
                    RawPluginAction::Add { specifier } => PluginAction::Add(specifier),
                    RawPluginAction::Remove { package } => PluginAction::Remove(package),
                },
            }),
            RawCommand::PluginReport => {
                if raw.data_dir.is_some() {
                    return Err(usage_error(
                        "the plugin-report command does not accept --data-dir",
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
                    data_dir: raw.data_dir,
                    patches: Vec::new(),
                }),
            })
        }
        Profile::Headless => {
            reject_launcher_options_after_task(&args)?;
            headless_command(raw.headless, raw.data_dir).map(|command| Cli {
                command: CliCommand::Headless(command),
            })
        }
    }
}

impl RawHeadlessCommand {
    fn is_set(&self) -> bool {
        self.session.is_some()
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
    data_dir: Option<PathBuf>,
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
        data_dir,
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

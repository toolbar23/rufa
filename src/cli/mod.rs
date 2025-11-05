use anyhow::{Result, bail};
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum, builder::PossibleValue};
use clap_complete::Shell;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "rufa", version, about = "Supervisor for coding-agent targets")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    pub async fn execute(self) -> Result<()> {
        match self.command {
            Command::Start(args) => handlers::start(args).await,
            Command::Run(args) => handlers::run(args).await,
            Command::Kill(args) => handlers::kill(args).await,
            Command::Info(args) => handlers::info(args).await,
            Command::Log(args) => handlers::log(args).await,
            Command::Restart(args) => handlers::restart(args).await,
            Command::Stop => handlers::stop().await,
            Command::Completions(args) => handlers::completions(args),
            Command::Daemon => handlers::daemon().await,
            Command::Init(args) => handlers::init(args).await,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Start the rufa daemon
    Start(StartArgs),
    /// Run one or more targets
    Run(RunArgs),
    /// Kill running targets
    Kill(KillArgs),
    /// Show info about running targets
    Info(InfoArgs),
    /// Stream logs from the supervisor
    Log(LogArgs),
    /// Restart running services
    Restart(RestartArgs),
    /// Stop all managed targets
    Stop,
    /// Generate shell completions
    Completions(CompletionsArgs),
    /// Internal daemon entry point (hidden)
    #[command(hide = true, alias = "__daemon")]
    Daemon,
    /// Prepare the repository to use rufa
    Init(InitArgs),
}

#[derive(Args, Debug)]
pub struct StartArgs {
    /// Enable watch mode by default for subsequent runs
    #[arg(long, short = 'w', action = ArgAction::SetTrue, conflicts_with = "no_watch")]
    pub watch: bool,

    /// Disable watch mode by default for subsequent runs
    #[arg(long = "no-watch", action = ArgAction::SetTrue, conflicts_with = "watch")]
    pub no_watch: bool,

    /// Keep the daemon attached to the terminal and stream logs
    #[arg(long, short = 'f')]
    pub foreground: bool,

    /// Load environment variables for the daemon from the given file
    #[arg(long = "env", short = 'e', value_name = "FILE")]
    pub env_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Targets to run
    #[arg(required = true)]
    pub targets: Vec<String>,

    /// Enable watch mode for these targets
    #[arg(long, short = 'w', action = ArgAction::SetTrue, conflicts_with = "no_watch")]
    pub watch: bool,

    /// Disable watch mode for these targets
    #[arg(long = "no-watch", action = ArgAction::SetTrue, conflicts_with = "watch")]
    pub no_watch: bool,

    /// Stay attached and stream logs for these targets
    #[arg(long, short = 'f')]
    pub foreground: bool,
}

#[derive(Args, Debug)]
pub struct KillArgs {
    /// Targets to terminate
    #[arg(required = true)]
    pub targets: Vec<String>,
}

#[derive(Args, Debug)]
pub struct InfoArgs {
    /// Optional specific targets to inspect
    #[arg()]
    pub targets: Vec<String>,

    /// Number of log lines to display per target in summaries
    #[arg(long = "log-length", short = 'n', default_value_t = 5)]
    pub log_length: usize,

    /// Continuously refresh the info view in the terminal
    #[arg(long, short = 'f')]
    pub foreground: bool,

    /// Number of previous runs to display
    #[arg(long = "history-length", default_value_t = 1)]
    pub history_length: usize,
}

#[derive(Args, Debug, Default)]
pub struct LogArgs {
    /// Targets to filter (default all)
    #[arg()]
    pub targets: Vec<String>,

    /// Follow log output
    #[arg(long, short = 'f')]
    pub follow: bool,

    /// Tail last N lines
    #[arg(long, short = 't')]
    pub tail: Option<usize>,

    /// Show specific generation
    #[arg(long, short = 'g', conflicts_with = "all")]
    pub generation: Option<u64>,

    /// Include all generations instead of the latest
    #[arg(long, short = 'a', conflicts_with = "generation")]
    pub all: bool,
}

#[derive(Args, Debug, Default)]
pub struct RestartArgs {
    /// Targets to restart (default none)
    #[arg()]
    pub targets: Vec<String>,

    /// Restart all running services
    #[arg(long, short = 'a')]
    pub all: bool,
}

#[derive(Args, Debug)]
pub struct CompletionsArgs {
    /// Shell to generate completions for
    #[arg(value_enum)]
    pub shell: CompletionShell,
}

#[derive(Args, Debug, Default)]
pub struct InitArgs {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
    Elvish,
}

impl ValueEnum for CompletionShell {
    fn value_variants<'a>() -> &'a [Self] {
        &[
            CompletionShell::Bash,
            CompletionShell::Zsh,
            CompletionShell::Fish,
            CompletionShell::PowerShell,
            CompletionShell::Elvish,
        ]
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(match self {
            CompletionShell::Bash => PossibleValue::new("bash"),
            CompletionShell::Zsh => PossibleValue::new("zsh"),
            CompletionShell::Fish => PossibleValue::new("fish"),
            CompletionShell::PowerShell => PossibleValue::new("powershell"),
            CompletionShell::Elvish => PossibleValue::new("elvish"),
        })
    }
}

impl From<CompletionShell> for Shell {
    fn from(shell: CompletionShell) -> Self {
        match shell {
            CompletionShell::Bash => Shell::Bash,
            CompletionShell::Zsh => Shell::Zsh,
            CompletionShell::Fish => Shell::Fish,
            CompletionShell::PowerShell => Shell::PowerShell,
            CompletionShell::Elvish => Shell::Elvish,
        }
    }
}

mod handlers;

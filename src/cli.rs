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

mod handlers {
    use super::*;
    use std::{
        borrow::Cow,
        collections::{HashMap, HashSet},
        fs, io,
        io::Write,
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };

    use crate::config;
    use crate::ipc::{
        BehaviorKind, ConfigureRequest, InfoRequest, InfoResponse, KillRequest, RestartRequest,
        RestartStatusSummary, RunHistorySummary, RunRequest, RunningTargetSummary, ServerResponse,
        StoppedTargetSummary, TargetConfigSummary, TargetRunState, WatchPreferenceKind,
        connect_existing_client, require_daemon_client, run_daemon, spawn_daemon_process,
        wait_for_daemon_ready, wait_for_shutdown,
    };
    use crate::runner::load_env_overrides;
    use anyhow::Context;
    use parking_lot::Mutex;
    use std::io::IsTerminal;
    use terminal_size::terminal_size;
    use tokio::{
        io::{AsyncReadExt, AsyncSeekExt},
        signal,
        time::{Duration as TokioDuration, sleep},
    };
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    pub async fn start(args: StartArgs) -> Result<()> {
        let StartArgs {
            watch,
            no_watch,
            foreground,
            env_file,
        } = args;

        let watch_setting = if watch {
            Some(true)
        } else if no_watch {
            Some(false)
        } else {
            None
        };

        let existing = connect_existing_client().await?;

        let env_overrides = if let Some(path) = env_file {
            if existing.is_some() {
                bail!("rufa daemon already running; stop it before restarting with --env");
            }
            let overrides = load_env_overrides(&path).with_context(|| {
                format!("loading environment overrides from {}", path.display())
            })?;
            Some(overrides)
        } else {
            None
        };

        if foreground {
            if existing.is_some() {
                println!("rufa daemon is already running; stop it before using --foreground");
                return Ok(());
            }

            let default_watch = watch_setting.unwrap_or(false);
            let previous_env = if let Some(overrides) = env_overrides.as_ref() {
                Some(apply_env(overrides)?)
            } else {
                None
            };

            unsafe {
                std::env::set_var("RUFA_DEFAULT_WATCH", if default_watch { "1" } else { "0" });
            }

            let log_path = resolve_log_path();
            let mut start_offset = 0;
            if let Ok(meta) = fs::metadata(&log_path) {
                start_offset = meta.len();
            }
            let empty_targets: Vec<String> = Vec::new();
            let filters =
                LogFilters::new(&empty_targets, None, true, HashMap::<String, u64>::new())?;

            println!("starting rufa daemon in foreground (press Ctrl+C to stop)");
            println!("streaming logs to stdout...");
            let follow_task = tokio::spawn({
                let filters = filters.clone();
                async move {
                    if let Err(error) = follow_log_stream(log_path, filters, start_offset).await {
                        eprintln!("log streaming terminated: {error}");
                    }
                }
            });

            let result = run_daemon().await;
            follow_task.abort();
            let _ = follow_task.await;
            unsafe {
                std::env::remove_var("RUFA_DEFAULT_WATCH");
            }
            if let Some(prev) = previous_env {
                restore_env(prev);
            }
            return result;
        }

        if let Some(client) = existing {
            if let Some(value) = watch_setting {
                match client
                    .configure(ConfigureRequest {
                        default_watch: Some(value),
                    })
                    .await?
                {
                    ServerResponse::Ack => {
                        println!(
                            "updated default watch setting to {} on running daemon",
                            if value { "enabled" } else { "disabled" }
                        );
                        Ok(())
                    }
                    ServerResponse::Info(_) => Ok(()),
                    ServerResponse::Error(message) => bail!(message),
                }
            } else {
                println!("rufa daemon already running; use `rufa stop` to restart it if needed");
                Ok(())
            }
        } else {
            let mut previous_env = None;
            if let Some(overrides) = env_overrides.as_ref() {
                previous_env = Some(apply_env(overrides)?);
            }

            let spawn_result = spawn_daemon_process(watch_setting.unwrap_or(false)).await;
            if let Err(error) = spawn_result {
                if let Some(prev) = previous_env.take() {
                    restore_env(prev);
                }
                return Err(error).context("failed to spawn rufa daemon");
            }

            let ready_result = wait_for_daemon_ready(Duration::from_secs(5)).await;
            if let Some(prev) = previous_env.take() {
                restore_env(prev);
            }
            ready_result.context("daemon did not become ready in time")?;
            println!("rufa daemon started; launch targets with `rufa run`");
            Ok(())
        }
    }

    pub async fn run(args: RunArgs) -> Result<()> {
        let RunArgs {
            targets,
            watch,
            no_watch,
            foreground,
        } = args;
        let display_targets = targets.clone();

        let watch_setting = if watch {
            Some(true)
        } else if no_watch {
            Some(false)
        } else {
            None
        };

        let client = require_daemon_client().await?;
        let log_path = resolve_log_path();
        let start_offset = if foreground {
            fs::metadata(&log_path).map(|meta| meta.len()).unwrap_or(0)
        } else {
            0
        };
        let request = RunRequest {
            targets,
            watch: watch_setting,
        };
        match client.run(request).await? {
            ServerResponse::Ack => {
                if foreground {
                    println!(
                        "running targets in foreground: {} (Ctrl+C to stop)",
                        display_targets.join(", ")
                    );

                    let filters = LogFilters::new(&display_targets, None, true, HashMap::new())?;

                    let mut follow_future = Box::pin(follow_log_stream(
                        log_path.clone(),
                        filters.clone(),
                        start_offset,
                    ));

                    tokio::select! {
                        res = &mut follow_future => {
                            if let Err(error) = res {
                                eprintln!("log streaming ended: {error}");
                            }
                        }
                        _ = signal::ctrl_c() => {
                            println!();
                        }
                    }

                    drop(follow_future);

                    let kill_request = KillRequest {
                        targets: display_targets.clone(),
                    };
                    match client.kill(kill_request).await {
                        Ok(ServerResponse::Ack | ServerResponse::Info(_)) => {
                            println!(
                                "sent kill signal for targets: {}",
                                display_targets.join(", ")
                            );
                            Ok(())
                        }
                        Ok(ServerResponse::Error(message)) => bail!(message),
                        Err(error) => Err(error).context("failed to send kill request"),
                    }
                } else {
                    println!(
                        "requested start for targets: {}",
                        display_targets.join(", ")
                    );
                    Ok(())
                }
            }
            ServerResponse::Info(_) => {
                println!("run request returned additional info:");
                Ok(())
            }
            ServerResponse::Error(message) => bail!(message),
        }
    }

    pub async fn kill(args: KillArgs) -> Result<()> {
        let KillArgs { targets } = args;
        let display_targets = targets.clone();
        let client = require_daemon_client().await?;
        let request = KillRequest { targets };
        match client.kill(request).await? {
            ServerResponse::Ack => {
                println!(
                    "requested termination for targets: {}",
                    display_targets.join(", ")
                );
                Ok(())
            }
            ServerResponse::Info(_) => Ok(()),
            ServerResponse::Error(message) => bail!(message),
        }
    }

    pub async fn info(args: InfoArgs) -> Result<()> {
        let client = require_daemon_client().await?;
        let request = InfoRequest {
            targets: args.targets.clone(),
        };

        if args.foreground {
            let mut stdout = std::io::stdout();
            println!("press Ctrl+C to exit foreground info view");
            let mut last_render = String::new();
            loop {
                let response = client.info(request.clone()).await;
                let rendered = match response {
                    Ok(ServerResponse::Info(data)) => {
                        match format_info_response(&data, args.log_length, args.history_length) {
                            Ok(text) => text,
                            Err(error) => {
                                eprintln!("failed to format info: {error}");
                                break;
                            }
                        }
                    }
                    Ok(ServerResponse::Ack) => {
                        "info command acknowledged (no details returned)\n".to_string()
                    }
                    Ok(ServerResponse::Error(message)) => {
                        eprintln!("daemon error: {message}");
                        break;
                    }
                    Err(error) => {
                        eprintln!("failed to fetch info: {error}");
                        break;
                    }
                };

                if rendered != last_render {
                    print!("\x1b[2J\x1b[H{rendered}");
                    stdout.flush().ok();
                    last_render = rendered;
                }

                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                    _ = signal::ctrl_c() => {
                        println!();
                        break;
                    }
                }
            }
            Ok(())
        } else {
            match client.info(request).await? {
                ServerResponse::Info(data) => {
                    let mut stdout = std::io::stdout();
                    write_info_response(&mut stdout, &data, args.log_length, args.history_length)?;
                    Ok(())
                }
                ServerResponse::Ack => {
                    println!("info command acknowledged (no details returned)");
                    Ok(())
                }
                ServerResponse::Error(message) => bail!(message),
            }
        }
    }

    pub async fn log(args: LogArgs) -> Result<()> {
        let log_path = resolve_log_path();
        println!("log file: {}", log_path.display());

        let (contents, file_len) = match fs::read_to_string(&log_path) {
            Ok(data) => {
                let len = fs::metadata(&log_path)
                    .map(|meta| meta.len())
                    .unwrap_or_else(|_| data.len() as u64);
                (Some(data), len)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                println!("log file does not exist yet");
                (None, 0)
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading log file {}", log_path.display()));
            }
        };

        let latest_generations = contents
            .as_deref()
            .map(|body| compute_latest_generations(body.lines()))
            .unwrap_or_default();

        let filters =
            LogFilters::new(&args.targets, args.generation, args.all, latest_generations)?;

        if let Some(body) = contents.as_ref() {
            print_log_snapshot(body, &filters, args.tail);
        }

        let start_offset = file_len;

        if !args.follow {
            return Ok(());
        }

        println!("-- following logs (press Ctrl+C to stop) --");
        let follow_filters = filters.clone();
        let follow_path = log_path.clone();

        tokio::select! {
            result = follow_log_stream(follow_path, follow_filters, start_offset) => result,
            _ = signal::ctrl_c() => {
                println!();
                Ok(())
            }
        }
    }

    pub async fn restart(args: RestartArgs) -> Result<()> {
        if args.all && !args.targets.is_empty() {
            bail!("cannot combine explicit targets with --all");
        }
        let RestartArgs { targets, all } = args;

        let client = require_daemon_client().await?;
        let request = RestartRequest {
            targets: targets.clone(),
            all,
        };
        match client.restart(request).await? {
            ServerResponse::Ack => {
                if all {
                    println!("requested restart for all services");
                } else if targets.is_empty() {
                    println!("restart request accepted (no explicit targets supplied)");
                } else {
                    println!(
                        "restart request accepted for targets: {}",
                        targets.join(", ")
                    );
                }
                Ok(())
            }
            ServerResponse::Info(_) => {
                println!("restart request returned additional data (not yet handled)");
                Ok(())
            }
            ServerResponse::Error(message) => bail!(message),
        }
    }

    pub async fn stop() -> Result<()> {
        let Some(client) = connect_existing_client().await? else {
            println!("rufa daemon is not running");
            return Ok(());
        };
        match client.stop().await? {
            ServerResponse::Ack => {
                wait_for_shutdown(Duration::from_secs(5)).await?;
                println!("rufa daemon stopped");
                Ok(())
            }
            ServerResponse::Info(_) => {
                wait_for_shutdown(Duration::from_secs(5)).await?;
                println!("rufa daemon stopped");
                Ok(())
            }
            ServerResponse::Error(message) => bail!(message),
        }
    }

    pub fn completions(args: CompletionsArgs) -> Result<()> {
        use clap::CommandFactory;
        let mut cmd = crate::cli::Cli::command();
        let shell: Shell = args.shell.into();
        clap_complete::generate(shell, &mut cmd, "rufa", &mut std::io::stdout());
        Ok(())
    }

    pub async fn daemon() -> Result<()> {
        run_daemon().await
    }

    fn write_info_response<W: Write>(
        mut writer: W,
        data: &InfoResponse,
        log_lines: usize,
        history_limit: usize,
    ) -> io::Result<()> {
        let terminal_width = detect_terminal_width();
        let colorize = std::io::stdout().is_terminal();
        writeln!(writer, "Running targets:")?;
        if data.running.is_empty() {
            writeln!(writer, "  - none")?;
        } else {
            for target in &data.running {
                write_running_summary(
                    &mut writer,
                    target,
                    log_lines,
                    history_limit,
                    terminal_width,
                    colorize,
                )?;
            }
        }

        writeln!(writer)?;
        writeln!(writer, "Stopped targets:")?;
        if data.stopped.is_empty() {
            writeln!(writer, "  - none")?;
        } else {
            for target in &data.stopped {
                write_stopped_summary(
                    &mut writer,
                    target,
                    log_lines,
                    history_limit,
                    terminal_width,
                    colorize,
                )?;
            }
        }

        writeln!(writer)?;
        writeln!(writer, "Other available targets:")?;
        if data.available.is_empty() {
            writeln!(writer, "  - none")?;
        } else {
            for target in &data.available {
                writeln!(
                    writer,
                    "  - {} ({})",
                    target.name,
                    behavior_label(target.kind)
                )?;
            }
        }

        Ok(())
    }

    fn write_running_summary<W: Write>(
        writer: &mut W,
        target: &RunningTargetSummary,
        log_lines: usize,
        history_limit: usize,
        terminal_width: Option<usize>,
        colorize: bool,
    ) -> io::Result<()> {
        writeln!(
            writer,
            "  - {} ({})",
            target.name,
            behavior_label(target.kind)
        )?;
        write_config_details(writer, &target.config)?;
        writeln!(
            writer,
            "      current run: {}",
            format_generation_heading(target.current.generation, true, colorize)
        )?;
        write_run_state(
            writer,
            &target.name,
            &target.current,
            "        ",
            log_lines,
            terminal_width,
            colorize,
        )?;
        write_history_section(
            writer,
            "previous runs",
            &target.name,
            &target.history,
            log_lines,
            history_limit,
            terminal_width,
            colorize,
        )?;
        Ok(())
    }

    fn format_info_response(
        data: &InfoResponse,
        log_lines: usize,
        history_limit: usize,
    ) -> io::Result<String> {
        let mut buffer = Vec::new();
        write_info_response(&mut buffer, data, log_lines, history_limit)?;
        String::from_utf8(buffer).map_err(|error| io::Error::new(io::ErrorKind::Other, error))
    }

    fn write_stopped_summary<W: Write>(
        writer: &mut W,
        target: &StoppedTargetSummary,
        log_lines: usize,
        history_limit: usize,
        terminal_width: Option<usize>,
        colorize: bool,
    ) -> io::Result<()> {
        writeln!(
            writer,
            "  - {} ({})",
            target.name,
            behavior_label(target.kind)
        )?;
        write_config_details(writer, &target.config)?;
        if let Some(restart) = target.restart.as_ref() {
            write_restart_status(writer, "      ", restart, colorize)?;
        }
        match &target.last {
            Some(last) => {
                write_run_snapshot(
                    writer,
                    &target.name,
                    last,
                    "last run",
                    "      ",
                    log_lines,
                    terminal_width,
                    colorize,
                )?;
            }
            None => writeln!(writer, "      last run: none")?,
        }
        write_history_section(
            writer,
            "previous runs",
            &target.name,
            &target.history,
            log_lines,
            history_limit,
            terminal_width,
            colorize,
        )?;
        Ok(())
    }

    fn write_config_details<W: Write>(
        writer: &mut W,
        config: &TargetConfigSummary,
    ) -> io::Result<()> {
        writeln!(writer, "      config:")?;
        writeln!(writer, "        driver: {}", config.driver)?;
        writeln!(
            writer,
            "        watch: {}",
            watch_preference_label(config.watch)
        )?;
        if config.watch_paths.is_empty() {
            writeln!(writer, "        watch paths: (default)")?;
        } else {
            writeln!(writer, "        watch paths:")?;
            for path in &config.watch_paths {
                writeln!(writer, "          - {}", path)?;
            }
        }
        Ok(())
    }

    fn write_run_state<W: Write>(
        writer: &mut W,
        target_name: &str,
        state: &TargetRunState,
        indent: &str,
        log_lines: usize,
        terminal_width: Option<usize>,
        colorize: bool,
    ) -> io::Result<()> {
        match state.pid {
            Some(pid) => writeln!(writer, "{indent}pid: {}", pid)?,
            None => writeln!(writer, "{indent}pid: n/a")?,
        }
        if state.ports.is_empty() {
            writeln!(writer, "{indent}ports: none")?;
        } else {
            writeln!(writer, "{indent}ports:")?;
            for port in &state.ports {
                writeln!(writer, "{indent}  {}", format_port_summary(port))?;
            }
        }
        if let Some(restart) = state.restart.as_ref() {
            write_restart_status(writer, indent, restart, colorize)?;
        }
        write_last_logs(
            writer,
            target_name,
            Some(state.generation),
            indent,
            log_lines,
            terminal_width,
            colorize,
        )?;
        if let Some(exit) = &state.last_exit {
            writeln!(writer, "{indent}last exit: {}", format_exit_summary(exit))?;
        }
        Ok(())
    }

    fn write_restart_status<W: Write>(
        writer: &mut W,
        indent: &str,
        restart: &RestartStatusSummary,
        colorize: bool,
    ) -> io::Result<()> {
        let mut parts = Vec::new();
        parts.push(format!("attempt {}", restart.attempt.max(1)));
        if let Some(delay) = restart.delay_seconds {
            parts.push(format!("next in {:.1}s", delay));
        }
        if let Some(at) = &restart.scheduled_for {
            parts.push(format!("scheduled for {}", at));
        }

        let label = if colorize {
            "\u{001b}[33mrestart pending\u{001b}[0m"
        } else {
            "restart pending"
        };

        writeln!(writer, "{indent}{label}: {}", parts.join(", "))
    }

    fn write_history_section<W: Write>(
        writer: &mut W,
        label: &str,
        target_name: &str,
        history: &[RunHistorySummary],
        log_lines: usize,
        history_limit: usize,
        terminal_width: Option<usize>,
        colorize: bool,
    ) -> io::Result<()> {
        if history.is_empty() || history_limit == 0 {
            writeln!(writer, "      {label}: none")?;
        } else {
            let singular = label.strip_suffix('s').unwrap_or(label).trim_end();
            for (index, record) in history
                .iter()
                .take(history_limit)
                .enumerate()
            {
                let heading = if history.len() == 1 {
                    singular.to_string()
                } else {
                    format!("{} {}", singular, index + 1)
                };
                write_run_snapshot(
                    writer,
                    target_name,
                    record,
                    &heading,
                    "      ",
                    log_lines,
                    terminal_width,
                    colorize,
                )?;
            }
        }
        Ok(())
    }

    fn write_run_snapshot<W: Write>(
        writer: &mut W,
        target_name: &str,
        record: &RunHistorySummary,
        heading: &str,
        indent: &str,
        log_lines: usize,
        terminal_width: Option<usize>,
        colorize: bool,
    ) -> io::Result<()> {
        writeln!(
            writer,
            "{indent}{heading}: {}",
            format_generation_heading(record.generation, false, colorize)
        )?;
        let nested_indent = format!("{indent}  ");
        if let Some(exit) = &record.exit {
            writeln!(
                writer,
                "{nested_indent}last exit: {}",
                format_exit_summary(exit)
            )?;
        }
        write_last_logs(
            writer,
            target_name,
            Some(record.generation),
            &nested_indent,
            log_lines,
            terminal_width,
            colorize,
        )?;
        Ok(())
    }

    fn format_generation_heading(generation: u64, is_current: bool, colorize: bool) -> String {
        if is_current {
            if colorize {
                format!("generation {} \u{001b}[32m[CURRENT]\u{001b}[0m", generation)
            } else {
                format!("generation {} [CURRENT]", generation)
            }
        } else {
            format!("generation {}", generation)
        }
    }

    fn write_last_logs<W: Write>(
        writer: &mut W,
        target: &str,
        generation: Option<u64>,
        indent: &str,
        limit: usize,
        terminal_width: Option<usize>,
        _colorize: bool,
    ) -> io::Result<()> {
        if limit == 0 {
            return Ok(());
        }

        match tail_logs_for_target(target, generation, limit) {
            Ok(lines) => {
                if lines.is_empty() {
                    writeln!(writer, "{indent}last logs: none")?;
                } else {
                    if lines.len() == 1 {
                        writeln!(writer, "{indent}last log:")?;
                    } else {
                        writeln!(writer, "{indent}last logs:")?;
                    }
                    let available_width = terminal_width.and_then(|width| {
                        let prefix_width = UnicodeWidthStr::width(indent).saturating_add(2);
                        width.checked_sub(prefix_width)
                    });
                    for log_line in lines {
                        let trimmed = log_line.trim_end();
                        let truncated = maybe_truncate_line(trimmed, available_width);
                        let colored = colorize_log_line(truncated.as_ref());
                        writeln!(writer, "{indent}  {colored}")?;
                    }
                }
            }
            Err(error) => {
                writeln!(writer, "{indent}last logs: <error reading log: {error}>")?;
            }
        }

        Ok(())
    }

    fn maybe_truncate_line<'a>(line: &'a str, max_width: Option<usize>) -> Cow<'a, str> {
        match max_width {
            None => Cow::Borrowed(line),
            Some(0) => Cow::Owned(String::new()),
            Some(limit) => {
                if UnicodeWidthStr::width(line) <= limit {
                    Cow::Borrowed(line)
                } else {
                    let mut acc = String::with_capacity(line.len());
                    let mut width = 0;
                    for ch in line.chars() {
                        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                        if width + ch_width > limit {
                            break;
                        }
                        acc.push(ch);
                        width += ch_width;
                    }
                    Cow::Owned(acc)
                }
            }
        }
    }

    fn detect_terminal_width() -> Option<usize> {
        if std::io::stdout().is_terminal() {
            terminal_size().map(|(width, _)| width.0 as usize)
        } else {
            None
        }
    }

    fn tail_logs_for_target(
        target: &str,
        generation: Option<u64>,
        limit: usize,
    ) -> io::Result<Vec<String>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let path = resolve_log_path();
        let contents = fs::read_to_string(&path)?;
        let mut matches = Vec::new();
        for line in contents.lines() {
            let columns: Vec<&str> = line.splitn(4, '|').collect();
            if columns.len() < 4 {
                continue;
            }
            if !columns[0].trim().eq_ignore_ascii_case(target) {
                continue;
            }
            if let Some(requested_generation) = generation {
                if columns[1].trim().parse::<u64>().ok() != Some(requested_generation) {
                    continue;
                }
            }
            matches.push(line.to_string());
        }

        let start = matches.len().saturating_sub(limit);
        Ok(matches.into_iter().skip(start).collect())
    }

    fn watch_preference_label(kind: WatchPreferenceKind) -> &'static str {
        match kind {
            WatchPreferenceKind::Rufa => "rufa",
            WatchPreferenceKind::Runtime => "runtime",
        }
    }

    fn behavior_label(kind: BehaviorKind) -> &'static str {
        match kind {
            BehaviorKind::Service => "service",
            BehaviorKind::Job => "job",
        }
    }

    fn format_port_summary(port: &crate::ipc::PortSummary) -> String {
        let mut segments = Vec::new();
        segments.push(format!("{} -> {}", port.name, port.port));
        if let Some(protocol) = &port.protocol {
            segments.push(protocol.clone());
        }
        if port.is_debug {
            segments.push("debug".to_string());
        }
        if let Some(url) = &port.url {
            segments.push(url.clone());
        }
        segments.join(" | ")
    }

    fn format_exit_summary(exit: &crate::ipc::ExitDetails) -> String {
        let mut parts = Vec::new();
        if let Some(code) = exit.code {
            parts.push(format!("code {}", code));
        }
        if let Some(signal) = exit.signal {
            parts.push(format!("signal {}", signal));
        }
        if let Some(at) = &exit.at {
            parts.push(at.clone());
        }
        if parts.is_empty() {
            "unknown".to_string()
        } else {
            parts.join(", ")
        }
    }

    fn apply_env(overrides: &HashMap<String, String>) -> Result<Vec<(String, Option<String>)>> {
        let mut previous = Vec::with_capacity(overrides.len());
        for (key, value) in overrides {
            let prior = std::env::var(key).ok();
            previous.push((key.clone(), prior));
            unsafe {
                std::env::set_var(key, value);
            }
        }
        Ok(previous)
    }

    fn restore_env(previous: Vec<(String, Option<String>)>) {
        for (key, value) in previous {
            unsafe {
                match value {
                    Some(val) => std::env::set_var(&key, val),
                    None => std::env::remove_var(&key),
                }
            }
        }
    }

    fn resolve_log_path() -> PathBuf {
        let config_path = Path::new("rufa.toml");
        match config::load_from_path(config_path) {
            Ok(cfg) => cfg.log.file,
            Err(_) => PathBuf::from("rufa.log"),
        }
    }

    fn print_log_snapshot(contents: &str, filters: &LogFilters, tail: Option<usize>) {
        let mut lines = filtered_lines(contents.lines(), filters);
        if let Some(count) = tail {
            if lines.len() > count {
                let drain_count = lines.len() - count;
                lines.drain(0..drain_count);
            }
        }

        for line in lines {
            println!("{}", colorize_log_line(&line));
        }
    }

    async fn follow_log_stream(
        log_path: PathBuf,
        filters: LogFilters,
        mut offset: u64,
    ) -> Result<()> {
        use tokio::{fs::File, io::SeekFrom};

        loop {
            match tokio::fs::metadata(&log_path).await {
                Ok(meta) => {
                    if meta.len() < offset {
                        offset = 0;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    offset = 0;
                    sleep(TokioDuration::from_millis(500)).await;
                    continue;
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("accessing {}", log_path.display()));
                }
            }

            let mut file = match File::open(&log_path).await {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    offset = 0;
                    sleep(TokioDuration::from_millis(500)).await;
                    continue;
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("opening {}", log_path.display()));
                }
            };

            file.seek(SeekFrom::Start(offset)).await?;

            let mut buffer = String::new();
            let bytes_read = file.read_to_string(&mut buffer).await?;
            if bytes_read > 0 {
                for line in buffer.lines() {
                    if let Some(parsed) = parse_log_line(line) {
                        filters.update_latest(&parsed);
                        if filters.matches(&parsed) {
                            println!("{}", colorize_log_line(line));
                        }
                    } else if filters.allow_unparsed() {
                        println!("{}", colorize_log_line(line));
                    }
                }
                offset += bytes_read as u64;
            }

            sleep(TokioDuration::from_millis(500)).await;
        }
    }

    #[derive(Clone)]
    struct LogFilters {
        targets: HashSet<String>,
        generation: Option<u64>,
        allow_all_generations: bool,
        latest: Arc<Mutex<HashMap<String, u64>>>,
    }

    impl LogFilters {
        fn new(
            targets: &[String],
            generation: Option<u64>,
            all_generations: bool,
            latest: HashMap<String, u64>,
        ) -> Result<Self> {
            if generation.is_some() && all_generations {
                bail!("cannot combine --generation with --all");
            }

            let normalized = targets
                .iter()
                .map(|value| value.to_ascii_uppercase())
                .collect::<HashSet<_>>();
            Ok(Self {
                targets: normalized,
                generation,
                allow_all_generations: all_generations,
                latest: Arc::new(Mutex::new(latest)),
            })
        }

        fn matches(&self, parsed: &ParsedLogLine) -> bool {
            if let Some(filter) = self.generation {
                if parsed.generation != Some(filter) {
                    return false;
                }
            } else if !self.allow_all_generations {
                if let Some(generation) = parsed.generation {
                    let latest = self.latest.lock();
                    match latest.get(parsed.target_upper.as_str()) {
                        Some(current) if generation == *current => {}
                        Some(_) => return false,
                        None => return false,
                    }
                } else {
                    return false;
                }
            }

            if self.targets.is_empty() {
                return true;
            }

            self.targets.contains(parsed.target_upper.as_str())
        }

        fn update_latest(&self, parsed: &ParsedLogLine) {
            if self.allow_all_generations || self.generation.is_some() {
                return;
            }

            if let Some(generation) = parsed.generation {
                let mut latest = self.latest.lock();
                let entry = latest
                    .entry(parsed.target_upper.clone())
                    .or_insert(generation);
                if generation > *entry {
                    *entry = generation;
                }
            }
        }

        fn allow_unparsed(&self) -> bool {
            self.targets.is_empty() && (self.allow_all_generations || self.generation.is_none())
        }
    }

    struct ParsedLogLine {
        generation: Option<u64>,
        target_upper: String,
    }

    fn parse_log_line(line: &str) -> Option<ParsedLogLine> {
        let mut parts = line.split('|');
        let target = parts.next()?.trim();
        let generation = parts
            .next()
            .and_then(|value| value.trim().parse::<u64>().ok());
        Some(ParsedLogLine {
            generation,
            target_upper: target.to_ascii_uppercase(),
        })
    }

    fn filtered_lines<'a>(
        lines: impl Iterator<Item = &'a str>,
        filters: &LogFilters,
    ) -> Vec<String> {
        let mut results = Vec::new();
        for line in lines {
            if let Some(parsed) = parse_log_line(line) {
                if filters.matches(&parsed) {
                    results.push(line.to_string());
                }
            } else if filters.allow_unparsed() {
                results.push(line.to_string());
            }
        }
        results
    }

    fn compute_latest_generations<'a>(
        lines: impl Iterator<Item = &'a str>,
    ) -> HashMap<String, u64> {
        let mut map = HashMap::new();
        for line in lines {
            if let Some(parsed) = parse_log_line(line) {
                if let Some(generation) = parsed.generation {
                    let key = parsed.target_upper;
                    let entry = map.entry(key).or_insert(generation);
                    if generation > *entry {
                        *entry = generation;
                    }
                }
            }
        }
        map
    }

    fn colorize_log_line(line: &str) -> String {
        lazy_static::lazy_static! {
            static ref LOG_LEVEL_PATTERN: regex::Regex =
                regex::Regex::new(r"(?P<prefix>[\s\[\(])(?P<level>ERROR|WARN|INFO)(?P<suffix>[\s\]\)])")
                    .expect("valid log level regex");
        }

        let parts: Vec<&str> = line.splitn(4, '|').collect();
        let mut colored = String::with_capacity(line.len() + 32);
        if parts.len() == 4 {
            for (idx, part) in parts.iter().enumerate() {
                if idx < 3 {
                    colored.push_str("\u{001b}[38;5;244m");
                    colored.push_str(part);
                    colored.push_str("\u{001b}[0m");
                    colored.push('|');
                } else {
                    let level_colored = apply_level_color(part);
                    if level_colored == *part {
                        colored.push_str("\u{001b}[38;5;244m");
                        colored.push_str(part);
                        colored.push_str("\u{001b}[0m");
                    } else {
                        colored.push_str(&level_colored);
                    }
                }
            }
        } else {
            let level_colored = apply_level_color(line);
            if level_colored == line {
                colored.push_str("\u{001b}[38;5;244m");
                colored.push_str(line);
                colored.push_str("\u{001b}[0m");
            } else {
                colored.push_str(&level_colored);
            }
        }
        colored
    }
}

fn apply_level_color(segment: &str) -> String {
    lazy_static::lazy_static! {
        static ref LOG_LEVEL_PATTERN: regex::Regex =
            regex::Regex::new(r"(?P<prefix>[ \t\(\[])(?P<level>ERROR|WARN|INFO)(?P<suffix>[ \t\)\]])")
                .expect("valid log level regex");
    }

    let mut result = String::with_capacity(segment.len() + 16);
    let mut last = 0usize;
    for mat in LOG_LEVEL_PATTERN.captures_iter(segment) {
        let m = mat.get(0).expect("match present");
        result.push_str(&segment[last..m.start()]);
        let prefix = mat.name("prefix").unwrap().as_str();
        let level = mat.name("level").unwrap().as_str();
        let suffix = mat.name("suffix").unwrap().as_str();

        result.push_str(prefix);
        let colored_level = match level {
            "ERROR" => "\u{001b}[31mERROR\u{001b}[0m",
            "WARN" => "\u{001b}[33mWARN\u{001b}[0m",
            "INFO" => "\u{001b}[32mINFO\u{001b}[0m",
            _ => level,
        };
        result.push_str(colored_level);
        result.push_str(suffix);

        last = m.end();
    }
    result.push_str(&segment[last..]);
    result
}

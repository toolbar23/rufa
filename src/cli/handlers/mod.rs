use super::log_io;
use super::*;
use crate::ipc::{
    BehaviorKind, InfoRequest, InfoResponse, RefreshCommand, RefreshMode, RefreshWatchTypeKind,
    RestartRequest, RunHistorySummary, RunningTargetSummary, ServerResponse, StartTargetsRequest,
    StopTargetsRequest, StoppedTargetSummary, TargetConfigSummary, TargetRunState,
    connect_existing_client, require_daemon_client, run_daemon, spawn_daemon_process,
    wait_for_daemon_ready, wait_for_shutdown,
};
use crate::{config, env::OVERRIDE_ENV_FILE_VAR};
use anyhow::Context;
use std::io::IsTerminal;
use std::{
    collections::HashMap, fs, future::Future, io, io::Write, path::Path, pin::Pin, time::Duration,
};
use terminal_size::terminal_size;
use tokio::{
    signal,
    time::{Duration as TokioDuration, sleep},
};
use unicode_width::UnicodeWidthStr;

use crate::paths;

pub async fn init(_args: InitArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("determining working directory")?;

    handle_rufa_toml(&cwd)?;
    handle_gitignore(&cwd)?;
    handle_agents_md(&cwd)?;

    Ok(())
}

pub async fn refresh(command: RefreshCliCommand) -> Result<()> {
    let client = require_daemon_client().await?;
    match command {
        RefreshCliCommand::Set(args) => {
            let mode = match args.mode {
                RefreshModeArg::Auto => RefreshMode::Auto,
                RefreshModeArg::Off => RefreshMode::Off,
            };
            match client
                .refresh(RefreshCommand::Set { mode })
                .await
                .context("failed to update refresh setting")?
            {
                ServerResponse::Ack => {
                    println!(
                        "refresh on change set to {}",
                        match mode {
                            RefreshMode::Auto => "auto",
                            RefreshMode::Off => "off",
                        }
                    );
                    Ok(())
                }
                ServerResponse::Info(_) => Ok(()),
                ServerResponse::Error(message) => bail!(message),
            }
        }
        RefreshCliCommand::StaleTargets => match client
            .refresh(RefreshCommand::RestartStaleTargets)
            .await
            .context("failed to refresh stale targets")?
        {
            ServerResponse::Ack => {
                println!("requested restart for stale refresh targets");
                Ok(())
            }
            ServerResponse::Info(_) => Ok(()),
            ServerResponse::Error(message) => bail!(message),
        },
    }
}

pub async fn start_daemon(args: DaemonStartArgs) -> Result<()> {
    let DaemonStartArgs {
        foreground,
        env_file,
    } = args;

    let env_file_path = if let Some(path) = env_file {
        let resolved = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .context("resolving relative --env path")?
                .join(path)
        };
        Some(resolved)
    } else {
        None
    };

    let existing = connect_existing_client().await?;

    if existing.is_some() && env_file_path.is_some() {
        bail!("rufa daemon already running; stop it before restarting with --env");
    }

    if foreground {
        if existing.is_some() {
            println!("rufa daemon is already running; stop it before using --foreground");
            return Ok(());
        }

        match env_file_path.as_ref() {
            Some(path) => unsafe { std::env::set_var(OVERRIDE_ENV_FILE_VAR, path) },
            None => unsafe { std::env::remove_var(OVERRIDE_ENV_FILE_VAR) },
        }

        println!("starting rufa daemon in foreground (press Ctrl+C to stop)");
        println!("logs directory: {}", paths::logs_dir().display());

        let result = run_daemon().await;
        unsafe {
            std::env::remove_var(OVERRIDE_ENV_FILE_VAR);
        }
        return result;
    }

    if existing.is_some() {
        println!("rufa daemon already running; use `rufa stop` to restart it if needed");
        Ok(())
    } else {
        let refresh_on_change = match config::load_from_path("rufa.toml") {
            Ok(cfg) => cfg.watch.refresh_on_change,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "failed to read refresh settings from rufa.toml; defaulting to auto"
                );
                true
            }
        };
        let spawn_result = spawn_daemon_process(refresh_on_change, env_file_path.as_deref()).await;
        if let Err(error) = spawn_result {
            return Err(error).context("failed to spawn rufa daemon");
        }

        let ready_result = wait_for_daemon_ready(Duration::from_secs(5)).await;
        ready_result.context("daemon did not become ready in time")?;
        println!("rufa daemon started; launch targets with `rufa target start`");
        Ok(())
    }
}

pub async fn start_targets(args: TargetStartArgs) -> Result<()> {
    let TargetStartArgs {
        targets,
        foreground,
    } = args;
    let display_targets = targets.clone();

    let client = require_daemon_client().await?;
    let request = StartTargetsRequest { targets };
    match client.start_targets(request).await? {
        ServerResponse::Ack => {
            if foreground {
                println!(
                    "running targets in foreground: {} (Ctrl+C to stop)",
                    display_targets.join(", ")
                );

                let info_request = InfoRequest {
                    targets: display_targets.clone(),
                };
                let generation_map = match client.info(info_request).await? {
                    ServerResponse::Info(response) => response
                        .generations
                        .into_iter()
                        .map(|item| (item.name, item.generation))
                        .collect(),
                    ServerResponse::Error(message) => bail!(message),
                    ServerResponse::Ack => HashMap::new(),
                };

                let sources =
                    log_io::build_sources(&display_targets, None, false, &generation_map)?;

                println!("logs directory: {}", paths::logs_dir().display());
                let mut follow_future: Pin<Box<dyn Future<Output = Result<()>> + Send>> =
                    if sources.is_empty() {
                        Box::pin(async {
                            loop {
                                sleep(TokioDuration::from_millis(500)).await;
                            }
                            #[allow(unreachable_code)]
                            Ok::<(), anyhow::Error>(())
                        })
                    } else {
                        Box::pin(log_io::follow_sources(sources))
                    };

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

                let stop_request = StopTargetsRequest {
                    targets: display_targets.clone(),
                };
                match client.stop_targets(stop_request).await {
                    Ok(ServerResponse::Ack | ServerResponse::Info(_)) => {
                        println!(
                            "sent stop signal for targets: {}",
                            display_targets.join(", ")
                        );
                        Ok(())
                    }
                    Ok(ServerResponse::Error(message)) => bail!(message),
                    Err(error) => Err(error).context("failed to send stop request"),
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
            println!("start request returned additional info:");
            Ok(())
        }
        ServerResponse::Error(message) => bail!(message),
    }
}

pub async fn stop_targets(args: TargetStopArgs) -> Result<()> {
    let TargetStopArgs { targets } = args;
    let display_targets = targets.clone();
    let client = require_daemon_client().await?;
    let request = StopTargetsRequest { targets };
    match client.stop_targets(request).await? {
        ServerResponse::Ack => {
            println!("requested stop for targets: {}", display_targets.join(", "));
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
    let mut generation_map = HashMap::new();
    let mut targets_request = args.targets.clone();

    if let Some(client) = connect_existing_client().await? {
        let info_request = InfoRequest {
            targets: if targets_request.is_empty() {
                Vec::new()
            } else {
                targets_request.clone()
            },
        };
        match client.info(info_request).await? {
            ServerResponse::Info(response) => {
                generation_map = response
                    .generations
                    .into_iter()
                    .map(|item| (item.name, item.generation))
                    .collect();
            }
            ServerResponse::Error(message) => bail!(message),
            ServerResponse::Ack => {}
        }
    } else {
        generation_map = log_io::read_generations_from_disk().unwrap_or_default();
    }

    if targets_request.is_empty() {
        targets_request = generation_map.keys().cloned().collect();
        targets_request.sort();
    }

    if targets_request.is_empty() {
        println!("no log sources available");
        return Ok(());
    }

    let sources =
        log_io::build_sources(&targets_request, args.generation, args.all, &generation_map)?;

    if sources.is_empty() {
        println!("no logs found for requested selection");
        return Ok(());
    }

    println!("logs directory: {}", paths::logs_dir().display());

    let mut entry_sets = Vec::new();
    for source in &sources {
        match log_io::read_entries(source) {
            Ok(entries) => entry_sets.push(entries),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", source.path().display()));
            }
        }
    }

    let mut merged = log_io::merge_entries(entry_sets);
    if let Some(tail) = args.tail {
        merged = log_io::take_last(merged, tail);
    }

    for entry in &merged {
        let formatted = log_io::format_entry(entry);
        println!("{}", log_io::colorize_line(&formatted));
    }

    if !args.follow {
        return Ok(());
    }

    println!("logs directory: {}", paths::logs_dir().display());
    println!("-- following logs (press Ctrl+C to stop) --");
    tokio::select! {
        result = log_io::follow_sources(sources) => result,
        _ = signal::ctrl_c() => {
            println!();
            Ok(())
        }
    }
}

pub async fn restart_targets(args: TargetRestartArgs) -> Result<()> {
    if args.all && !args.targets.is_empty() {
        bail!("cannot combine explicit targets with --all");
    }
    let TargetRestartArgs { targets, all } = args;

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

pub async fn stop_daemon() -> Result<()> {
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
    writeln!(
        writer,
        "Refresh on change: {}",
        if data.refresh_target_on_change_enabled {
            "auto"
        } else {
            "off"
        }
    )?;
    writeln!(writer)?;
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
    if let Some(control) = target.control.as_ref() {
        writeln!(writer, "      desired state: {}", control.desired_state)?;
        writeln!(writer, "      action plan: {}", control.action_plan)?;
    }
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
    if let Some(control) = target.control.as_ref() {
        writeln!(writer, "      desired state: {}", control.desired_state)?;
        writeln!(writer, "      action plan: {}", control.action_plan)?;
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

fn write_config_details<W: Write>(writer: &mut W, config: &TargetConfigSummary) -> io::Result<()> {
    writeln!(writer, "      config:")?;
    writeln!(writer, "        driver: {}", config.driver)?;
    writeln!(
        writer,
        "        refresh watch type: {}",
        refresh_watch_type_label(config.refresh_watch_type)
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
    writeln!(
        writer,
        "{indent}refresh pending: {}",
        if state.refresh_pending { "yes" } else { "no" }
    )?;
    write_last_logs(
        writer,
        target_name,
        state.generation,
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
        for (index, record) in history.iter().take(history_limit).enumerate() {
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
        record.generation,
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
    generation: u64,
    indent: &str,
    limit: usize,
    terminal_width: Option<usize>,
    _colorize: bool,
) -> io::Result<()> {
    if limit == 0 {
        return Ok(());
    }

    let available_width = terminal_width
        .and_then(|width| width.checked_sub(UnicodeWidthStr::width(indent).saturating_add(2)));

    match log_io::tail_logs_for_target(target, generation, limit, available_width) {
        Ok(lines) => {
            if lines.is_empty() {
                writeln!(writer, "{indent}last logs: none")?;
            } else {
                if lines.len() == 1 {
                    writeln!(writer, "{indent}last log:")?;
                } else {
                    writeln!(writer, "{indent}last logs:")?;
                }
                for line in lines {
                    let colored = log_io::colorize_line(&line);
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

fn detect_terminal_width() -> Option<usize> {
    if std::io::stdout().is_terminal() {
        terminal_size().map(|(width, _)| width.0 as usize)
    } else {
        None
    }
}

fn refresh_watch_type_label(kind: RefreshWatchTypeKind) -> &'static str {
    match kind {
        RefreshWatchTypeKind::Rufa => "rufa",
        RefreshWatchTypeKind::Runtime => "runtime",
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

fn handle_rufa_toml(root: &Path) -> Result<()> {
    let path = root.join("rufa.toml");
    if path.exists() {
        println!("rufa.toml already exists; skipping.");
        return Ok(());
    }

    let template = r#"# Generated by `rufa init`

[log]
file = "rufa.log"
rotation_after_seconds = 86400
rotation_after_size_mb = 10
keep_history_count = 5

[env]
read_env_file = ".env"

[watch]
stability_seconds = 2

[target.sample]
kind = "service"
type = "bash"
command = "echo 'replace me with your start command'"
"#;

    println!("Proposed rufa.toml:\n{}", template);
    if prompt_yes_no("Create rufa.toml with this content?")? {
        fs::write(&path, template)?;
        println!("Created {}", path.display());
    } else {
        println!("Skipped creating rufa.toml");
    }
    Ok(())
}

fn handle_gitignore(root: &Path) -> Result<()> {
    let path = root.join(".gitignore");
    let existing = if path.exists() {
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };

    let patterns = [".rufa/", "rufa.log", "rufa.log.*"];
    let mut missing = Vec::new();
    for pattern in patterns.into_iter() {
        if !existing.lines().any(|line| line.trim() == pattern) {
            missing.push(pattern);
        }
    }

    if missing.is_empty() {
        println!(".gitignore already ignores rufa artifacts.");
        return Ok(());
    }

    let has_comment = existing.lines().any(|line| line.trim() == "# rufa");
    let mut preview = String::new();
    if !has_comment {
        preview.push_str("# rufa\n");
    }
    for pattern in &missing {
        preview.push_str(pattern);
        preview.push('\n');
    }

    println!("Proposed .gitignore additions:\n{}", preview);
    if prompt_yes_no("Append these entries to .gitignore?")? {
        let mut new_contents = existing;
        if !new_contents.is_empty() && !new_contents.ends_with('\n') {
            new_contents.push('\n');
        }
        if !has_comment {
            new_contents.push_str("# rufa\n");
        }
        for pattern in &missing {
            new_contents.push_str(pattern);
            new_contents.push('\n');
        }
        fs::write(&path, new_contents)?;
        println!("Updated {}", path.display());
    } else {
        println!("Skipped updating .gitignore");
    }

    Ok(())
}

fn handle_agents_md(root: &Path) -> Result<()> {
    let path = root.join("AGENTS.md");
    let (mut existing, created) = if path.exists() {
        (
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?,
            false,
        )
    } else {
        (String::new(), true)
    };

    if existing.contains("## Working with rufa") {
        println!("AGENTS.md already documents rufa usage.");
        return Ok(());
    }

    let section = r#"## Working with rufa

- Start the supervisor once per repo: `rufa start --watch`.
- Launch targets as needed, for example `rufa target start sample`.
- Check `rufa info` for port assignments and active processes.
- Tail recent output with `rufa log --tail 20 --follow`.
- Update `.env` and rerun `rufa target start …` when environment changes.
"#;

    let mut preview = String::new();
    if created {
        preview.push_str("# Agent Guide\n\n");
    }
    preview.push_str(section);

    println!("Proposed AGENTS.md changes:\n{}", preview);
    if prompt_yes_no("Apply these AGENTS.md updates?")? {
        if created {
            fs::write(&path, preview)?;
        } else {
            if !existing.trim_end().is_empty() {
                existing.push_str("\n\n");
            }
            existing.push_str(section);
            fs::write(&path, existing)?;
        }
        println!("Updated {}", path.display());
    } else {
        println!("Skipped updating AGENTS.md");
    }

    Ok(())
}

fn prompt_yes_no(prompt: &str) -> Result<bool> {
    loop {
        print!("{} [y/N]: ", prompt);
        io::stdout().flush().ok();
        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            println!("Failed to read input; assuming 'no'.");
            return Ok(false);
        }
        match input.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" | "" => return Ok(false),
            _ => println!("Please enter 'y' or 'n'."),
        }
    }
}

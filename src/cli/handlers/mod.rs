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
use crate::env::OVERRIDE_ENV_FILE_VAR;
use crate::ipc::{
    BehaviorKind, ConfigureRequest, InfoRequest, InfoResponse, KillRequest, RestartRequest,
    RestartStatusSummary, RunHistorySummary, RunRequest, RunningTargetSummary, ServerResponse,
    StoppedTargetSummary, TargetConfigSummary, TargetRunState, WatchPreferenceKind,
    connect_existing_client, require_daemon_client, run_daemon, spawn_daemon_process,
    wait_for_daemon_ready, wait_for_shutdown,
};
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

pub async fn init(_args: InitArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("determining working directory")?;

    handle_rufa_toml(&cwd)?;
    handle_gitignore(&cwd)?;
    handle_agents_md(&cwd)?;

    Ok(())
}

pub async fn start(args: StartArgs) -> Result<()> {
    let StartArgs {
        watch,
        no_watch,
        foreground,
        env_file,
    } = args;

    let watch_setting = watch_setting_from_flags(watch, no_watch);

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

        let default_watch = watch_setting.unwrap_or(false);
        unsafe {
            std::env::set_var("RUFA_DEFAULT_WATCH", if default_watch { "1" } else { "0" });
        }
        match env_file_path.as_ref() {
            Some(path) => unsafe { std::env::set_var(OVERRIDE_ENV_FILE_VAR, path) },
            None => unsafe { std::env::remove_var(OVERRIDE_ENV_FILE_VAR) },
        }

        let log_path = resolve_log_path();
        let mut start_offset = 0;
        if let Ok(meta) = fs::metadata(&log_path) {
            start_offset = meta.len();
        }
        let empty_targets: Vec<String> = Vec::new();
        let filters = LogFilters::new(&empty_targets, None, true, HashMap::<String, u64>::new())?;

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
        unsafe {
            std::env::remove_var(OVERRIDE_ENV_FILE_VAR);
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
        let spawn_result =
            spawn_daemon_process(watch_setting.unwrap_or(false), env_file_path.as_deref()).await;
        if let Err(error) = spawn_result {
            return Err(error).context("failed to spawn rufa daemon");
        }

        let ready_result = wait_for_daemon_ready(Duration::from_secs(5)).await;
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

    let watch_setting = watch_setting_from_flags(watch, no_watch);

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
            return Err(error).with_context(|| format!("reading log file {}", log_path.display()));
        }
    };

    let latest_generations = contents
        .as_deref()
        .map(|body| compute_latest_generations(body.lines()))
        .unwrap_or_default();

    let filters = LogFilters::new(&args.targets, args.generation, args.all, latest_generations)?;

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

fn write_config_details<W: Write>(writer: &mut W, config: &TargetConfigSummary) -> io::Result<()> {
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

fn watch_setting_from_flags(watch: bool, no_watch: bool) -> Option<bool> {
    match (watch, no_watch) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
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

async fn follow_log_stream(log_path: PathBuf, filters: LogFilters, mut offset: u64) -> Result<()> {
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

fn filtered_lines<'a>(lines: impl Iterator<Item = &'a str>, filters: &LogFilters) -> Vec<String> {
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

fn compute_latest_generations<'a>(lines: impl Iterator<Item = &'a str>) -> HashMap<String, u64> {
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

[restart]
type = "BACKOFF"

[env]
read_env_file = ".env"

[watch]
stability = "2s"

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
- Launch targets as needed, for example `rufa run sample`.
- Check `rufa info` for port assignments and active processes.
- Tail recent output with `rufa log --tail 20 --follow`.
- Update `.env` and rerun `rufa run …` when environment changes.
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

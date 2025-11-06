use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};

use anyhow::{Context, Result, anyhow, bail};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::RwLock,
    time::sleep,
};

use crate::config::{self, Config, Target, TargetBehavior, WatchPreference};
use crate::ipc::{
    AvailableTargetSummary, BehaviorKind, ConfigureRequest, ControlStateSummary, ExitDetails,
    InfoRequest, InfoResponse, KillRequest, LockInfo, LogRequest, PortSummary, RestartRequest,
    RunRequest, lock,
    protocol::{
        ClientCommand, RunHistorySummary, RunningTargetSummary, ServerResponse,
        StoppedTargetSummary, TargetConfigSummary, TargetGenerationSummary, TargetRunState,
        WatchPreferenceKind,
    },
};
use crate::logging::EventLogger;
use crate::runner::Runner;
use crate::state::{ExitStatus, RuntimeState};
use crate::watch::WatchManager;

#[derive(Debug, Clone)]
pub struct IpcClient {
    lock: LockInfo,
}

impl IpcClient {
    pub fn new(lock: LockInfo) -> Self {
        Self { lock }
    }

    pub async fn send(&self, command: ClientCommand) -> Result<ServerResponse> {
        let stream = UnixStream::connect(self.lock.socket_path())
            .await
            .with_context(|| format!("failed to connect to {:?}", self.lock.socket_path()))?;

        send_command_over_stream(stream, command).await
    }

    pub async fn ping(&self) -> Result<ServerResponse> {
        self.send(ClientCommand::Ping).await
    }

    pub async fn run(&self, request: RunRequest) -> Result<ServerResponse> {
        self.send(ClientCommand::Run(request)).await
    }

    pub async fn kill(&self, request: KillRequest) -> Result<ServerResponse> {
        self.send(ClientCommand::Kill(request)).await
    }

    pub async fn configure(&self, request: ConfigureRequest) -> Result<ServerResponse> {
        self.send(ClientCommand::Configure(request)).await
    }

    pub async fn info(&self, request: InfoRequest) -> Result<ServerResponse> {
        self.send(ClientCommand::Info(request)).await
    }

    #[allow(dead_code)]
    pub async fn log(&self, request: LogRequest) -> Result<ServerResponse> {
        self.send(ClientCommand::Log(request)).await
    }

    pub async fn restart(&self, request: RestartRequest) -> Result<ServerResponse> {
        self.send(ClientCommand::Restart(request)).await
    }

    pub async fn stop(&self) -> Result<ServerResponse> {
        self.send(ClientCommand::Stop).await
    }
}

#[derive(Clone)]
struct ServerContext {
    runner: Arc<Runner>,
    watch: Arc<WatchManager>,
    default_watch: Arc<AtomicBool>,
}

pub async fn connect_existing_client() -> Result<Option<IpcClient>> {
    let Some(lock_info) = lock::read_lock().with_context(|| "reading .rufa.lock")? else {
        return Ok(None);
    };

    let client = IpcClient::new(lock_info.clone());
    match client.ping().await {
        Ok(ServerResponse::Ack) | Ok(ServerResponse::Info(_)) => Ok(Some(client)),
        Ok(ServerResponse::Error(message)) => {
            tracing::warn!(%message, "rufa daemon responded with error; cleaning up lock");
            cleanup_stale(&lock_info)?;
            Ok(None)
        }
        Err(error) => {
            tracing::warn!(%error, "failed to reach existing rufa daemon; cleaning up lock");
            cleanup_stale(&lock_info)?;
            Ok(None)
        }
    }
}

pub async fn require_daemon_client() -> Result<IpcClient> {
    connect_existing_client()
        .await?
        .ok_or_else(|| anyhow!("rufa daemon is not running; launch it with `rufa start`"))
}

pub async fn spawn_daemon_process(default_watch: bool, env_file: Option<&Path>) -> Result<()> {
    // Clean up any existing lock/socket artifacts before spawning.
    if let Some(lock_info) = lock::read_lock().ok().flatten() {
        let _ = cleanup_stale(&lock_info);
    }

    let current_exe = std::env::current_exe().context("locating rufa executable")?;
    tracing::info!("starting rufa daemon via {:?}", current_exe);

    let mut command = std::process::Command::new(current_exe);
    command.arg("__daemon");
    command.env("RUFA_DEFAULT_WATCH", if default_watch { "1" } else { "0" });
    match env_file {
        Some(path) => {
            command.env(crate::env::OVERRIDE_ENV_FILE_VAR, path);
        }
        None => {
            command.env_remove(crate::env::OVERRIDE_ENV_FILE_VAR);
        }
    }
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::inherit());
    command.stderr(std::process::Stdio::inherit());

    command.spawn().context("failed to spawn rufa daemon")?;
    Ok(())
}

pub async fn wait_for_daemon_ready(timeout: Duration) -> Result<IpcClient> {
    let start = Instant::now();
    loop {
        if let Some(client) = connect_existing_client().await? {
            return Ok(client);
        }

        if start.elapsed() >= timeout {
            bail!("rufa daemon did not become ready within {:?}", timeout);
        }

        sleep(Duration::from_millis(100)).await;
    }
}

pub async fn wait_for_shutdown(timeout: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        if lock::read_lock().ok().flatten().is_none() {
            return Ok(());
        }

        if start.elapsed() >= timeout {
            bail!("rufa daemon did not shut down within {:?}", timeout);
        }

        sleep(Duration::from_millis(100)).await;
    }
}

pub async fn run_daemon() -> Result<()> {
    run_daemon_internal().await
}

async fn run_daemon_internal() -> Result<()> {
    lock::ensure_runtime_dir().context("ensuring runtime directory")?;

    let socket_path = lock::allocate_socket_path().context("allocating socket path")?;
    if socket_path.exists() {
        lock::remove_socket(&socket_path)?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("binding IPC socket at {:?}", socket_path))?;

    let default_watch = match std::env::var("RUFA_DEFAULT_WATCH") {
        Ok(value) => matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"),
        Err(_) => false,
    };

    let lock_info = LockInfo {
        pid: process::id(),
        socket_path: socket_path.to_string_lossy().into_owned(),
        default_watch,
    };
    lock::write_lock(&lock_info).context("writing .rufa.lock")?;

    let guard = DaemonGuard::new(lock_info.clone());

    tracing::info!(
        socket = %lock_info.socket_path,
        pid = lock_info.pid,
        "rufa daemon started"
    );

    let config_path = PathBuf::from("rufa.toml");
    let env_override_path = std::env::var_os(crate::env::OVERRIDE_ENV_FILE_VAR).map(PathBuf::from);
    let watch = Arc::new(WatchManager::new());
    let default_watch_flag = Arc::new(AtomicBool::new(default_watch));
    let context = Arc::new(ServerContext {
        runner: initialize_runner(&config_path, env_override_path)
            .context("initializing runner")?,
        watch,
        default_watch: default_watch_flag,
    });

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("accepting IPC connection")?;
        let should_shutdown = handle_connection(stream, context.clone())
            .await
            .unwrap_or_else(|error| {
                tracing::error!(%error, "error handling IPC connection");
                false
            });

        if should_shutdown {
            tracing::info!("stop command received; shutting down daemon");
            break;
        }
    }

    if let Err(error) = context.runner.stop_all().await {
        tracing::warn!(%error, "failed to stop targets during shutdown");
    }

    context.watch.disable().await;

    drop(guard);
    Ok(())
}

fn cleanup_stale(lock_info: &LockInfo) -> Result<()> {
    tracing::debug!(
        socket = %lock_info.socket_path,
        "cleaning up stale rufa lock/socket"
    );
    lock::cleanup_lock(lock_info)
        .with_context(|| format!("cleaning up lock {:?}", lock::lock_file_path()))?;
    Ok(())
}

async fn send_command_over_stream(
    stream: UnixStream,
    command: ClientCommand,
) -> Result<ServerResponse> {
    let payload = serde_json::to_vec(&command)
        .map_err(|err| anyhow!("failed to serialize IPC command: {err}"))?;
    let (read_half, mut write_half) = stream.into_split();
    write_half.write_all(&payload).await?;
    write_half.write_all(b"\n").await?;
    write_half.shutdown().await?;

    let mut reader = BufReader::new(read_half);
    let mut response_line = String::new();
    let bytes = reader.read_line(&mut response_line).await?;
    if bytes == 0 {
        bail!("daemon closed connection without response");
    }
    let response: ServerResponse = serde_json::from_str(response_line.trim())
        .map_err(|err| anyhow!("failed to parse daemon response: {err}"))?;
    Ok(response)
}

async fn handle_connection(stream: UnixStream, context: Arc<ServerContext>) -> Result<bool> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let bytes = reader.read_line(&mut line).await?;
    if bytes == 0 {
        return Ok(false);
    }

    let command: ClientCommand = serde_json::from_str(line.trim())
        .map_err(|err| anyhow!("failed to parse command: {err}"))?;

    let response = match command {
        ClientCommand::Ping => ServerResponse::Ack,
        ClientCommand::Run(request) => {
            let watch = request
                .watch
                .unwrap_or_else(|| context.default_watch.load(Ordering::SeqCst));

            tracing::info!(
                targets = ?request.targets,
                watch,
                "received run request"
            );

            match context.runner.request_run(&request.targets, watch).await {
                Ok(outcome) => {
                    if let Some(spec) = outcome.rufa_watch {
                        if let Err(error) = context
                            .watch
                            .enable_watch(context.runner.clone(), spec)
                            .await
                        {
                            tracing::error!(%error, "failed to enable watch mode");
                            return Ok(false);
                        }
                    } else {
                        context.watch.disable().await;
                    }
                    ServerResponse::Ack
                }
                Err(error) => {
                    tracing::error!(%error, "failed to queue run request");
                    ServerResponse::Error(error.to_string())
                }
            }
        }
        ClientCommand::Info(request) => {
            tracing::info!(targets = ?request.targets, "received info request");
            let config_path = context.runner.config_path().to_path_buf();
            let config = match config::load_from_path(&config_path) {
                Ok(cfg) => cfg,
                Err(error) => {
                    tracing::error!(%error, path = ?config_path, "failed to load configuration");
                    write_response(
                        &mut write_half,
                        ServerResponse::Error(format!("failed to load configuration: {error}")),
                    )
                    .await?;
                    return Ok(false);
                }
            };

            let running = gather_running_targets(&context.runner, &config, &request.targets).await;
            let (running_summaries, running_names) = running;
            let (stopped_summaries, stopped_names) =
                gather_stopped_targets(&context.runner, &config, &request.targets, &running_names)
                    .await;
            let mut exclude = running_names;
            exclude.extend(stopped_names.iter().cloned());
            let available = gather_available_targets(&config, &request.targets, &exclude);
            let generations = context
                .runner
                .generation_snapshot()
                .await
                .into_iter()
                .map(|(name, generation)| TargetGenerationSummary { name, generation })
                .collect();
            let response = InfoResponse {
                running: running_summaries,
                stopped: stopped_summaries,
                available,
                generations,
            };
            write_response(&mut write_half, ServerResponse::Info(response)).await?;
            return Ok(false);
        }
        ClientCommand::Log(request) => {
            tracing::info!(
                targets = ?request.targets,
                follow = request.follow,
                tail = ?request.tail,
                generation = ?request.generation,
                "received log request (pending)"
            );
            ServerResponse::Ack
        }
        ClientCommand::Restart(request) => {
            tracing::info!(targets = ?request.targets, all = request.all, "received restart request");
            match context
                .runner
                .request_restart(&request.targets, request.all)
                .await
            {
                Ok(outcome) => {
                    if let Some(spec) = outcome.rufa_watch {
                        if let Err(error) = context
                            .watch
                            .enable_watch(context.runner.clone(), spec)
                            .await
                        {
                            tracing::error!(%error, "failed to enable watch mode after restart");
                        }
                    } else {
                        context.watch.disable().await;
                    }
                    ServerResponse::Ack
                }
                Err(error) => {
                    tracing::error!(%error, "failed to queue restart request");
                    ServerResponse::Error(error.to_string())
                }
            }
        }
        ClientCommand::Kill(request) => {
            tracing::info!(targets = ?request.targets, "received kill request");
            if request.targets.is_empty() {
                ServerResponse::Ack
            } else {
                match context.runner.kill_targets(&request.targets).await {
                    Ok(()) => ServerResponse::Ack,
                    Err(error) => {
                        tracing::error!(%error, "failed to kill targets");
                        ServerResponse::Error(error.to_string())
                    }
                }
            }
        }
        ClientCommand::Configure(request) => {
            if let Some(value) = request.default_watch {
                context.default_watch.store(value, Ordering::SeqCst);
                if let Err(error) = lock::update_default_watch(value) {
                    tracing::warn!(%error, "failed to update lock with new default watch setting");
                }
            }
            ServerResponse::Ack
        }
        ClientCommand::Stop => {
            tracing::info!("received stop request");
            let stop_targets = context.runner.stop_all().await;
            context.watch.disable().await;
            match stop_targets {
                Ok(()) => {
                    write_response(&mut write_half, ServerResponse::Ack).await?;
                    return Ok(true);
                }
                Err(error) => {
                    tracing::error!(%error, "failed to stop targets");
                    write_response(&mut write_half, ServerResponse::Error(error.to_string()))
                        .await?;
                    return Ok(false);
                }
            }
        }
    };

    write_response(&mut write_half, response).await?;
    Ok(false)
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: ServerResponse,
) -> Result<()> {
    let payload = serde_json::to_vec(&response)
        .map_err(|err| anyhow!("failed to serialize response: {err}"))?;
    writer.write_all(&payload).await?;
    writer.write_all(b"\n").await?;
    writer.shutdown().await?;
    Ok(())
}

async fn gather_running_targets(
    runner: &Runner,
    config: &Config,
    filter: &[String],
) -> (Vec<RunningTargetSummary>, std::collections::HashSet<String>) {
    let filter_set: std::collections::HashSet<_> = if filter.is_empty() {
        std::collections::HashSet::new()
    } else {
        filter
            .iter()
            .map(|name| name.to_ascii_uppercase())
            .collect()
    };

    let state_lock = runner.state();
    let state = state_lock.read().await;

    let mut names = std::collections::HashSet::new();
    let mut running = Vec::new();

    for (name, target_state) in state.targets.iter() {
        if target_state.pid.is_none() {
            continue;
        }

        if !filter_set.is_empty() && !filter_set.contains(&name.to_ascii_uppercase()) {
            continue;
        }

        let Some(Target::Unit(unit)) = config.targets.get(name) else {
            continue;
        };

        let kind = match unit.behavior {
            TargetBehavior::Service => BehaviorKind::Service,
            TargetBehavior::Job { .. } => BehaviorKind::Job,
        };

        let config_summary = TargetConfigSummary {
            driver: unit.driver_type.clone(),
            watch: watch_preference_kind(unit.watch_preference),
            watch_paths: unit
                .watch_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
        };

        let current = TargetRunState {
            generation: target_state.generation,
            pid: target_state.pid,
            ports: build_port_summaries(unit, &target_state.ports),
            last_log: target_state.last_log_line.clone(),
            last_exit: target_state.last_exit.as_ref().map(exit_details_from_state),
        };

        let history = target_state
            .history
            .iter()
            .map(run_history_summary)
            .collect();

        let control = runner
            .control_info(name)
            .await
            .map(|info| ControlStateSummary {
                desired_state: info.desired_state,
                action_plan: info.action_plan,
            });

        running.push(RunningTargetSummary {
            name: name.clone(),
            kind,
            config: config_summary,
            current,
            history,
            control,
        });

        names.insert(name.clone());
    }

    running.sort_by(|a, b| a.name.cmp(&b.name));

    (running, names)
}

async fn gather_stopped_targets(
    runner: &Runner,
    config: &Config,
    filter: &[String],
    exclude: &std::collections::HashSet<String>,
) -> (Vec<StoppedTargetSummary>, std::collections::HashSet<String>) {
    let filter_set: std::collections::HashSet<_> = if filter.is_empty() {
        std::collections::HashSet::new()
    } else {
        filter
            .iter()
            .map(|name| name.to_ascii_uppercase())
            .collect()
    };

    let state_lock = runner.state();
    let state = state_lock.read().await;

    let mut names = std::collections::HashSet::new();
    let mut stopped = Vec::new();

    for (name, target_state) in state.targets.iter() {
        if target_state.pid.is_some() || target_state.generation == 0 {
            continue;
        }

        if exclude.contains(name) {
            continue;
        }

        if !filter_set.is_empty() && !filter_set.contains(&name.to_ascii_uppercase()) {
            continue;
        }

        let Some(Target::Unit(unit)) = config.targets.get(name) else {
            continue;
        };

        let kind = match unit.behavior {
            TargetBehavior::Service => BehaviorKind::Service,
            TargetBehavior::Job { .. } => BehaviorKind::Job,
        };

        let config_summary = TargetConfigSummary {
            driver: unit.driver_type.clone(),
            watch: watch_preference_kind(unit.watch_preference),
            watch_paths: unit
                .watch_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
        };

        let last = Some(RunHistorySummary {
            generation: target_state.generation,
            last_log: target_state.last_log_line.clone(),
            exit: target_state.last_exit.as_ref().map(exit_details_from_state),
        });

        let history = target_state
            .history
            .iter()
            .map(run_history_summary)
            .collect();

        let control = runner
            .control_info(name)
            .await
            .map(|info| ControlStateSummary {
                desired_state: info.desired_state,
                action_plan: info.action_plan,
            });

        stopped.push(StoppedTargetSummary {
            name: name.clone(),
            kind,
            config: config_summary,
            last,
            history,
            control,
        });

        names.insert(name.clone());
    }

    stopped.sort_by(|a, b| a.name.cmp(&b.name));

    (stopped, names)
}

fn gather_available_targets(
    config: &Config,
    filter: &[String],
    exclude: &std::collections::HashSet<String>,
) -> Vec<AvailableTargetSummary> {
    let filter_set: std::collections::HashSet<_> = if filter.is_empty() {
        std::collections::HashSet::new()
    } else {
        filter
            .iter()
            .map(|name| name.to_ascii_uppercase())
            .collect()
    };

    config
        .targets
        .iter()
        .filter_map(|(name, target)| match target {
            Target::Unit(unit) => {
                if exclude.contains(name) {
                    return None;
                }
                if !filter_set.is_empty() && !filter_set.contains(&name.to_ascii_uppercase()) {
                    return None;
                }
                let kind = match unit.behavior {
                    TargetBehavior::Service => BehaviorKind::Service,
                    TargetBehavior::Job { .. } => BehaviorKind::Job,
                };
                Some(AvailableTargetSummary {
                    name: name.clone(),
                    kind,
                })
            }
            Target::Composite(_) => None,
        })
        .collect()
}

fn protocol_label(protocol: crate::config::model::PortProtocol) -> String {
    match protocol {
        crate::config::model::PortProtocol::Http => "http".to_string(),
        crate::config::model::PortProtocol::Tcp => "tcp".to_string(),
        crate::config::model::PortProtocol::Ftp => "ftp".to_string(),
    }
}

fn build_port_summaries(
    unit: &crate::config::UnitTarget,
    ports: &HashMap<String, u16>,
) -> Vec<PortSummary> {
    let mut summaries: Vec<PortSummary> = ports
        .iter()
        .map(|(port_name, port)| {
            let protocol = unit
                .ports
                .get(port_name)
                .map(|definition| protocol_label(definition.protocol));
            let is_debug = port_name.to_ascii_lowercase().contains("debug");
            let url = if matches!(protocol.as_deref(), Some("http")) {
                Some(format!("http://127.0.0.1:{port}"))
            } else {
                None
            };
            PortSummary {
                name: port_name.clone(),
                port: *port,
                protocol,
                is_debug,
                url,
            }
        })
        .collect();
    summaries.sort_by(|a, b| a.name.cmp(&b.name));
    summaries
}

fn watch_preference_kind(preference: WatchPreference) -> WatchPreferenceKind {
    match preference {
        WatchPreference::Rufa => WatchPreferenceKind::Rufa,
        WatchPreference::PreferRuntimeSupplied => WatchPreferenceKind::Runtime,
    }
}

fn run_history_summary(record: &crate::state::RunRecord) -> RunHistorySummary {
    RunHistorySummary {
        generation: record.generation,
        last_log: record.last_log_line.clone(),
        exit: record.exit.as_ref().map(exit_details_from_state),
    }
}

fn exit_details_from_state(exit: &ExitStatus) -> ExitDetails {
    let timestamp = DateTime::<Utc>::from(exit.when).to_rfc3339();
    ExitDetails {
        code: exit.code,
        signal: exit.signal,
        at: Some(timestamp),
    }
}

fn initialize_runner(
    config_path: &Path,
    env_override_path: Option<PathBuf>,
) -> Result<Arc<Runner>> {
    let (log_config, history_keep) = match config::load_from_path(config_path) {
        Ok(cfg) => (cfg.log, cfg.run_history.keep_runs),
        Err(error) => {
            tracing::warn!(%error, path = ?config_path, "using default logging configuration");
            (
                config::LogConfig::default(),
                config::RunHistoryConfig::default().keep_runs,
            )
        }
    };

    let logger = Arc::new(EventLogger::new(log_config).context("initializing event logger")?);
    let state = Arc::new(RwLock::new(RuntimeState::default()));
    let runner = Runner::new(
        config_path.to_path_buf(),
        logger,
        state,
        history_keep,
        env_override_path,
    )?;
    let runner = Arc::new(runner);
    runner.spawn_background_tasks();
    Ok(runner)
}

struct DaemonGuard {
    lock_info: LockInfo,
}

impl DaemonGuard {
    fn new(lock_info: LockInfo) -> Self {
        Self { lock_info }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = lock::cleanup_lock(&self.lock_info);
    }
}

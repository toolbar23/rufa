#![allow(dead_code)]

//! Runtime process launcher and supervisor.

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, anyhow, bail};
use nix::{
    errno::Errno,
    sys::signal::{Signal, kill as send_unix_signal},
    unistd::Pid,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::TcpListener as TokioTcpListener,
    process::Command,
    sync::{Mutex, Notify, RwLock, mpsc},
    task::{JoinHandle, yield_now},
    time::{sleep, timeout},
};

use crate::{
    config::{
        self, Config, EnvValue, ReferenceResource, RestartConfig, RestartStrategy, Target,
        TargetBehavior, TargetName, UnitTarget, WatchPreference,
    },
    debug_update::DebugUpdateContext,
    logging::{EventLogger, LogEvent, LogStream},
    state::{ExitStatus, RuntimeState, TargetState},
};

#[derive(Debug)]
pub struct Runner {
    config_path: PathBuf,
    logger: Arc<EventLogger>,
    state: Arc<RwLock<RuntimeState>>,
    port_allocator: Arc<Mutex<PortAllocator>>,
    processes: Arc<RwLock<HashMap<String, Arc<ProcessHandle>>>>,
    port_sentries: Arc<RwLock<HashMap<String, PortSentrySet>>>,
    restart_config: RestartConfig,
    restart_state: Arc<Mutex<HashMap<String, BackoffStatus>>>,
    restart_suppressed: Arc<Mutex<HashSet<String>>>,
    restart_tx: mpsc::Sender<RestartJob>,
    restart_rx: Mutex<mpsc::Receiver<RestartJob>>,
    history_keep: usize,
}

#[derive(Debug, Default, Clone)]
pub struct WatchSpec {
    pub services: HashMap<String, Vec<PathBuf>>,
    pub stability: Duration,
}

#[derive(Debug, Default)]
pub struct LaunchOutcome {
    pub rufa_watch: Option<WatchSpec>,
}

impl Runner {
    pub fn new(
        config_path: impl Into<PathBuf>,
        logger: Arc<EventLogger>,
        state: Arc<RwLock<RuntimeState>>,
        restart_config: RestartConfig,
        history_keep: usize,
    ) -> Self {
        let (restart_tx, restart_rx) = mpsc::channel(32);
        Self {
            config_path: config_path.into(),
            logger,
            state,
            port_allocator: Arc::new(Mutex::new(PortAllocator::default())),
            processes: Arc::new(RwLock::new(HashMap::new())),
            port_sentries: Arc::new(RwLock::new(HashMap::new())),
            restart_config,
            restart_state: Arc::new(Mutex::new(HashMap::new())),
            restart_suppressed: Arc::new(Mutex::new(HashSet::new())),
            restart_tx,
            restart_rx: Mutex::new(restart_rx),
            history_keep: history_keep.max(1),
        }
    }

    pub fn spawn_background_tasks(self: &Arc<Self>) {
        let runner = Arc::clone(self);
        tokio::spawn(async move {
            runner.restart_worker_loop().await;
        });
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn logger(&self) -> Arc<EventLogger> {
        self.logger.clone()
    }

    pub fn state(&self) -> Arc<RwLock<RuntimeState>> {
        self.state.clone()
    }

    async fn restart_worker_loop(self: Arc<Self>) {
        let mut rx = self.restart_rx.lock().await;
        while let Some(job) = rx.recv().await {
            self.process_restart_job(job).await;
        }
    }

    async fn process_restart_job(&self, job: RestartJob) {
        if job.delay > Duration::ZERO {
            sleep(job.delay).await;
        }

        if self.consume_suppression(&job.target).await {
            self.clear_pending_flag(&job.target).await;
            self.reset_backoff_state(&job.target).await;
            update_runtime_restart_state(&self.state, &job.target, false, 0, None, None).await;
            return;
        }

        match self.launch_targets(&[job.target.clone()], false).await {
            Ok(_) => {
                self.clear_pending_flag(&job.target).await;
            }
            Err(error) => {
                tracing::error!(%error, target = %job.target, attempt = job.attempt, "backoff restart failed to launch");
                self.clear_pending_flag(&job.target).await;
                let _ = self
                    .schedule_restart_with_attempt(&job.target, Some(job.attempt.saturating_add(1)))
                    .await;
            }
        }
    }

    async fn schedule_restart(&self, target: &str) {
        let _ = self.schedule_restart_with_attempt(target, None).await;
    }

    async fn schedule_restart_with_attempt(
        &self,
        target: &str,
        attempt_override: Option<u32>,
    ) -> Result<RestartSchedule, ()> {
        let outcome = schedule_restart_core(
            self.restart_config.strategy,
            target,
            attempt_override,
            &self.restart_state,
            &self.restart_suppressed,
            &self.restart_tx,
        )
        .await?;

        match outcome {
            RestartSchedule::Scheduled { attempt, delay } => {
                let scheduled_for = SystemTime::now().checked_add(delay);
                update_runtime_restart_state(
                    &self.state,
                    target,
                    true,
                    attempt,
                    Some(delay),
                    scheduled_for,
                )
                .await;
            }
            RestartSchedule::Skipped => {
                update_runtime_restart_state(&self.state, target, false, 0, None, None).await;
            }
            RestartSchedule::AlreadyPending => {}
        }

        Ok(outcome)
    }

    async fn clear_pending_flag(&self, target: &str) {
        clear_pending_flag_shared(&self.restart_state, target).await;
    }

    async fn reset_backoff_state(&self, target: &str) {
        reset_backoff_state_shared(&self.restart_state, target).await;
    }

    async fn consume_suppression(&self, target: &str) -> bool {
        consume_suppression_shared(&self.restart_suppressed, target).await
    }

    async fn mark_restart_suppressed(&self, target: &str) {
        mark_suppressed_shared(&self.restart_suppressed, target).await;
    }

    async fn on_target_launched(&self, target: &str, behavior: &TargetBehavior) {
        let _ = consume_suppression_shared(&self.restart_suppressed, target).await;
        if matches!(behavior, TargetBehavior::Service) {
            reset_backoff_state_shared(&self.restart_state, target).await;
            update_runtime_restart_state(&self.state, target, false, 0, None, None).await;
        }
    }

    pub async fn watch_spec(&self, targets: &[String]) -> Result<WatchSpec> {
        let config = config::load_from_path(&self.config_path).with_context(|| {
            format!(
                "loading configuration from {:?}",
                self.config_path.as_os_str()
            )
        })?;

        let base = self
            .config_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let base = fs::canonicalize(&base).unwrap_or(base);

        let units = resolve_units(&config, targets)?;
        let mut map = HashMap::new();
        for unit in units {
            if matches!(unit.behavior, TargetBehavior::Service) {
                let dirs = if unit.watch_paths.is_empty() {
                    vec![base.clone()]
                } else {
                    unit.watch_paths
                        .iter()
                        .map(|entry| {
                            if entry.is_absolute() {
                                entry.clone()
                            } else {
                                let joined = base.join(entry);
                                fs::canonicalize(&joined).unwrap_or(joined)
                            }
                        })
                        .collect()
                };
                map.insert(unit.name.clone(), dirs);
            }
        }

        Ok(WatchSpec {
            services: map,
            stability: config.watch.stability,
        })
    }

    pub async fn launch_targets(&self, targets: &[String], watch: bool) -> Result<LaunchOutcome> {
        let config = config::load_from_path(&self.config_path).with_context(|| {
            format!(
                "loading configuration from {:?}",
                self.config_path.as_os_str()
            )
        })?;

        let units = resolve_units(&config, targets)?;
        if units.is_empty() {
            bail!("no runnable targets resolved from selection");
        }

        {
            let processes = self.processes.read().await;
            for unit in &units {
                if processes.contains_key(&unit.name) {
                    bail!("target {} is already running", unit.name);
                }
            }
        }

        let debug_context = config.debug_update.clone().map(|settings| {
            Arc::new(DebugUpdateContext::new(
                self.config_path.clone(),
                settings,
                self.state.clone(),
            ))
        });

        let env_file_vars = match config.env.read_env_file.as_ref() {
            Some(path) => {
                if !path.exists() {
                    tracing::warn!(
                        path = %path.display(),
                        "environment file not found; continuing without overrides"
                    );
                    HashMap::new()
                } else {
                    match load_env_overrides(path) {
                        Ok(vars) => vars,
                        Err(error) => {
                            tracing::warn!(%error, "failed to load environment overrides");
                            HashMap::new()
                        }
                    }
                }
            }
            None => HashMap::new(),
        };

        let assignments = self.allocate_ports(&units).await?;
        let state_snapshot = {
            let guard = self.state.read().await;
            guard.clone()
        };

        let mut rufa_watch_targets = Vec::new();

        for unit in units {
            let ports = assignments.get(&unit.name).cloned().unwrap_or_default();

            let generation = state_snapshot
                .targets
                .get(&unit.name)
                .map(|state| state.generation + 1)
                .unwrap_or(1);

            let previous = state_snapshot.targets.get(&unit.name);
            let wants_runtime_watch = if watch {
                matches!(
                    unit.watch_preference,
                    WatchPreference::PreferRuntimeSupplied
                )
            } else {
                previous.map(|state| state.runtime_watch).unwrap_or(false)
            };
            let wants_rufa_watch = if watch {
                matches!(unit.watch_preference, WatchPreference::Rufa)
            } else {
                previous.map(|state| state.rufa_watch).unwrap_or(false)
            };
            if wants_rufa_watch {
                rufa_watch_targets.push(unit.name.clone());
            }

            let command_line = resolve_command(&unit, wants_runtime_watch)
                .with_context(|| format!("determining launch command for {}", unit.name))?;

            let env = build_environment(
                &unit,
                generation,
                &env_file_vars,
                &ports,
                &assignments,
                &state_snapshot,
            )?;

            self.ensure_port_sentries(&unit.name, &ports)
                .await
                .with_context(|| {
                    format!(
                        "establishing placeholder port listeners for target {}",
                        unit.name
                    )
                })?;

            if let Err(error) = self
                .spawn_target(
                    unit.clone(),
                    generation,
                    command_line,
                    ports.clone(),
                    env,
                    debug_context.clone(),
                    wants_runtime_watch,
                    wants_rufa_watch,
                )
                .await
            {
                let mut allocator = self.port_allocator.lock().await;
                allocator.release_ports(ports.values().copied());
                return Err(error);
            }

            if let Some(ctx) = &debug_context {
                ctx.refresh()
                    .await
                    .with_context(|| format!("updating debug configurations for {}", unit.name))?;
            }
        }

        let rufa_watch = if rufa_watch_targets.is_empty() {
            None
        } else {
            Some(self.watch_spec(&rufa_watch_targets).await?)
        };

        Ok(LaunchOutcome { rufa_watch })
    }

    async fn ensure_port_sentries(&self, target: &str, ports: &HashMap<String, u16>) -> Result<()> {
        {
            let guard = self.port_sentries.read().await;
            if guard.contains_key(target) {
                return Ok(());
            }
        }

        let sentries = PortSentrySet::bind_from_ports(ports)
            .await
            .with_context(|| format!("binding placeholder listeners for target {}", target))?;

        let mut map = self.port_sentries.write().await;
        if map.contains_key(target) {
            drop(map);
            sentries.stop().await;
            return Ok(());
        }
        map.insert(target.to_string(), sentries);
        Ok(())
    }

    async fn restore_placeholder_listeners(
        &self,
        target: &str,
        ports: HashMap<String, u16>,
    ) -> Result<()> {
        restore_port_sentries(self.port_sentries.clone(), target.to_string(), ports).await
    }

    async fn allocate_ports(
        &self,
        units: &[UnitTarget],
    ) -> Result<HashMap<TargetName, HashMap<String, u16>>> {
        let mut allocator = self.port_allocator.lock().await;
        let mut assignments = HashMap::new();
        for unit in units {
            let mut ports = HashMap::new();
            for (name, definition) in &unit.ports {
                let port = allocator
                    .allocate(definition.selection)
                    .with_context(|| format!("allocating port for {}.{}", unit.name, name))?;
                ports.insert(name.clone(), port);
            }
            assignments.insert(unit.name.clone(), ports);
        }
        Ok(assignments)
    }

    async fn spawn_target(
        &self,
        unit: UnitTarget,
        generation: u64,
        command_line: String,
        ports: HashMap<String, u16>,
        env: HashMap<String, String>,
        debug_context: Option<Arc<DebugUpdateContext>>,
        runtime_watch: bool,
        rufa_watch: bool,
    ) -> Result<()> {
        let mut command = Command::new("bash");
        command.arg("-lc").arg(&command_line);
        command.envs(&env);
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        command.stdin(std::process::Stdio::null());

        let target_name = unit.name.clone();

        if let Some(guards) = {
            let mut map = self.port_sentries.write().await;
            map.remove(&target_name)
        } {
            guards.stop().await;
        }

        let mut child = match command
            .spawn()
            .with_context(|| format!("spawning target {}", target_name))
        {
            Ok(child) => child,
            Err(error) => {
                if let Err(err) = self
                    .restore_placeholder_listeners(&target_name, ports.clone())
                    .await
                {
                    tracing::warn!(%err, target = %target_name, "failed to restore listeners after spawn error");
                }
                return Err(error);
            }
        };
        let pid = child.id().map(|value| value as u32);

        {
            let mut state = self.state.write().await;
            let entry = state
                .targets
                .entry(target_name.clone())
                .or_insert_with(TargetState::default);
            entry.record_snapshot(self.history_keep);
            entry.reset_for_new_run(generation, ports.clone(), runtime_watch, rufa_watch);
            entry.pid = pid;
            entry.restart_attempt = 0;
            entry.restart_pending = false;
        }

        let behavior = unit.behavior.clone();
        self.on_target_launched(&target_name, &behavior).await;

        let process_handle = Arc::new(ProcessHandle::new());
        {
            let mut processes = self.processes.write().await;
            if processes
                .insert(target_name.clone(), process_handle.clone())
                .is_some()
            {
                bail!("target {} is already running", target_name);
            }
        }

        self.logger
            .log(
                LogEvent::new(target_name.clone(), generation, LogStream::Started)
                    .with_message(command_line.clone()),
            )
            .ok();

        if let Some(stdout) = child.stdout.take() {
            self.spawn_stream_task(target_name.clone(), generation, LogStream::Stdout, stdout);
        }

        if let Some(stderr) = child.stderr.take() {
            self.spawn_stream_task(target_name.clone(), generation, LogStream::Stderr, stderr);
        }

        let logger = self.logger.clone();
        let state = self.state.clone();
        let allocator = self.port_allocator.clone();
        let processes = self.processes.clone();
        let restart_state = self.restart_state.clone();
        let restart_suppressed = self.restart_suppressed.clone();
        let restart_tx = self.restart_tx.clone();
        let restart_strategy = self.restart_config.strategy;
        let timeout = match &behavior {
            TargetBehavior::Job { timeout } => *timeout,
            TargetBehavior::Service => None,
        };
        let ports_for_wait = ports.clone();
        let debug_context_task = debug_context.clone();
        let process_handle_wait = process_handle.clone();
        let port_sentries = self.port_sentries.clone();
        tokio::spawn(async move {
            let mut forced_message: Option<String> = None;
            let wait_result = if let Some(limit) = timeout {
                match tokio::time::timeout(limit, child.wait()).await {
                    Ok(result) => result,
                    Err(_) => {
                        forced_message = Some("terminated due to timeout".to_string());
                        if let Err(error) = child.kill().await {
                            tracing::warn!(%error, target = %target_name, "failed to kill timed-out job");
                        }
                        child.wait().await
                    }
                }
            } else {
                child.wait().await
            };

            let mut should_restart = false;
            match wait_result {
                Ok(exit_status) => {
                    let message = forced_message.unwrap_or_else(|| exit_message(&exit_status));
                    if let Err(error) = logger.log(
                        LogEvent::new(target_name.clone(), generation, LogStream::Exited)
                            .with_message(&message),
                    ) {
                        tracing::error!(%error, target = %target_name, "failed to log exit event");
                    }

                    let exit_snapshot = ExitStatus {
                        code: exit_status.code(),
                        signal: exit_signal(&exit_status),
                        when: SystemTime::now(),
                    };

                    let mut state_guard = state.write().await;
                    if let Some(entry) = state_guard.targets.get_mut(&target_name) {
                        entry.pid = None;
                        entry.last_exit = Some(exit_snapshot);
                        entry.restart_attempt = 0;
                        entry.restart_pending = false;
                    }

                    if matches!(behavior, TargetBehavior::Service) && !exit_status.success() {
                        should_restart = true;
                    }
                }
                Err(error) => {
                    tracing::error!(%error, target = %target_name, "child wait failed");
                    let mut state_guard = state.write().await;
                    if let Some(entry) = state_guard.targets.get_mut(&target_name) {
                        entry.pid = None;
                        entry.restart_pending = false;
                    }
                    if matches!(behavior, TargetBehavior::Service) {
                        should_restart = true;
                    }
                }
            }

            if matches!(behavior, TargetBehavior::Service) {
                if should_restart {
                    match schedule_restart_core(
                        restart_strategy,
                        &target_name,
                        None,
                        &restart_state,
                        &restart_suppressed,
                        &restart_tx,
                    )
                    .await
                    {
                        Ok(RestartSchedule::Scheduled { attempt, delay }) => {
                            let scheduled_for = SystemTime::now().checked_add(delay);
                            update_runtime_restart_state(
                                &state,
                                &target_name,
                                true,
                                attempt,
                                Some(delay),
                                scheduled_for,
                            )
                            .await;
                        }
                        Ok(RestartSchedule::AlreadyPending) => {}
                        Ok(RestartSchedule::Skipped) => {
                            update_runtime_restart_state(
                                &state,
                                &target_name,
                                false,
                                0,
                                None,
                                None,
                            )
                            .await;
                        }
                        Err(_) => {
                            update_runtime_restart_state(
                                &state,
                                &target_name,
                                false,
                                0,
                                None,
                                None,
                            )
                            .await;
                        }
                    }
                } else {
                    reset_backoff_state_shared(&restart_state, &target_name).await;
                    update_runtime_restart_state(&state, &target_name, false, 0, None, None).await;
                }
            }

            if let Err(error) = restore_port_sentries(
                port_sentries.clone(),
                target_name.clone(),
                ports_for_wait.clone(),
            )
            .await
            {
                tracing::warn!(%error, target = %target_name, "failed to restore placeholder listeners after process exit");
            }

            let mut allocator_guard = allocator.lock().await;
            allocator_guard.release_ports(ports_for_wait.values().copied());

            if let Some(ctx) = debug_context_task {
                if let Err(error) = ctx.refresh().await {
                    tracing::warn!(
                        %error,
                        target = %target_name,
                        "failed to update debug configurations after target exit"
                    );
                }
            }

            process_handle_wait.signal_exit();
            let mut processes_guard = processes.write().await;
            processes_guard.remove(&target_name);
        });

        Ok(())
    }

    fn spawn_stream_task<R>(&self, target: String, generation: u64, stream: LogStream, reader: R)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        let logger = self.logger.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Err(error) = logger
                    .log(LogEvent::new(target.clone(), generation, stream).with_message(&line))
                {
                    tracing::error!(%error, target = %target, "failed to log stream line");
                }

                let mut state_guard = state.write().await;
                if let Some(entry) = state_guard.targets.get_mut(&target) {
                    entry.last_log_line = Some(line);
                }
            }
        });
    }

    pub async fn restart_targets(&self, targets: &[String], all: bool) -> Result<LaunchOutcome> {
        let config = config::load_from_path(&self.config_path).with_context(|| {
            format!(
                "loading configuration from {:?}",
                self.config_path.as_os_str()
            )
        })?;

        let unit_names = if all {
            let processes = self.processes.read().await;
            if processes.is_empty() {
                bail!("no services are currently running");
            }
            processes.keys().cloned().collect::<Vec<_>>()
        } else {
            if targets.is_empty() {
                bail!("specify at least one target or use --all");
            }
            targets.to_vec()
        };

        let units = resolve_units(&config, &unit_names)?;
        if units.is_empty() {
            bail!("no runnable targets resolved from restart selection");
        }

        for unit in &units {
            if matches!(unit.behavior, TargetBehavior::Job { .. }) {
                bail!(
                    "target {} is a job; re-run it explicitly instead of restarting",
                    unit.name
                );
            }
        }

        let state_snapshot = self.state.read().await.clone();
        let watch_requested = state_snapshot
            .targets
            .iter()
            .filter(|(name, _)| unit_names.contains(name))
            .any(|(_, state)| state.rufa_watch || state.runtime_watch);
        drop(state_snapshot);

        for unit in &unit_names {
            self.terminate_target(unit).await?;
        }

        self.launch_targets(&unit_names, watch_requested).await
    }

    pub async fn stop_all(&self) -> Result<()> {
        let targets = {
            let processes = self.processes.read().await;
            processes.keys().cloned().collect::<Vec<_>>()
        };

        for target in targets {
            self.terminate_target(&target).await?;
        }

        Ok(())
    }

    pub async fn kill_targets(&self, targets: &[String]) -> Result<()> {
        for target in targets {
            self.terminate_target(target).await?;
        }
        Ok(())
    }

    async fn terminate_target(&self, target: &str) -> Result<()> {
        let handle = {
            let processes = self.processes.read().await;
            processes.get(target).cloned()
        };

        let Some(handle) = handle else {
            return Ok(());
        };

        self.mark_restart_suppressed(target).await;

        let pid = {
            let state_guard = self.state.read().await;
            state_guard.targets.get(target).and_then(|state| state.pid)
        };

        if let Some(pid) = pid {
            if let Err(error) = signal_process(pid, Signal::SIGTERM) {
                tracing::warn!(%error, target, "failed to send SIGTERM");
            }
            if timeout(Duration::from_secs(5), handle.wait_for_exit())
                .await
                .is_err()
            {
                tracing::warn!(
                    target,
                    "process did not exit after SIGTERM; escalating to SIGKILL"
                );
                if let Err(error) = signal_process(pid, Signal::SIGKILL) {
                    tracing::error!(%error, target, "failed to send SIGKILL");
                }
                let _ = timeout(Duration::from_secs(5), handle.wait_for_exit()).await;
            }
        }

        {
            let mut state_guard = self.state.write().await;
            if let Some(entry) = state_guard.targets.get_mut(target) {
                entry.runtime_watch = false;
                entry.rufa_watch = false;
                entry.restart_pending = false;
                entry.restart_attempt = 0;
                entry.restart_delay = None;
                entry.restart_scheduled_for = None;
            }
        }

        Ok(())
    }
}

fn resolve_units(config: &Config, targets: &[String]) -> Result<Vec<UnitTarget>> {
    let mut resolved = Vec::new();
    let mut visiting = HashSet::new();

    for name in targets {
        expand_target(config, name, &mut resolved, &mut visiting)?;
    }

    Ok(resolved)
}

fn expand_target(
    config: &Config,
    name: &str,
    resolved: &mut Vec<UnitTarget>,
    visiting: &mut HashSet<String>,
) -> Result<()> {
    if !visiting.insert(name.to_string()) {
        bail!("cycle detected while expanding composite target {name}");
    }

    let Some(target) = config.targets.get(name) else {
        bail!("target {name} not found in configuration");
    };

    match target {
        Target::Composite(composite) => {
            for member in &composite.members {
                expand_target(config, member, resolved, visiting)?;
            }
        }
        Target::Unit(unit) => {
            resolved.push(unit.clone());
        }
    }

    visiting.remove(name);
    Ok(())
}

fn build_environment(
    unit: &UnitTarget,
    generation: u64,
    env_file_vars: &HashMap<String, String>,
    my_ports: &HashMap<String, u16>,
    assignments: &HashMap<String, HashMap<String, u16>>,
    state: &RuntimeState,
) -> Result<HashMap<String, String>> {
    let mut env = HashMap::new();
    env.extend(env_file_vars.clone());

    for (key, value) in &unit.env {
        let rendered = match value {
            EnvValue::Literal(literal) => literal.clone(),
            EnvValue::Reference(reference) => resolve_reference(reference, assignments, state)?,
        };
        env.insert(key.clone(), rendered);
    }

    for (name, port) in my_ports {
        let upper = name.to_ascii_uppercase();
        env.insert(format!("RUFA_PORT_{upper}"), port.to_string());
        env.insert(format!("PORT_{upper}"), port.to_string());
    }

    env.insert("RUFA_TARGET".to_string(), unit.name.clone());
    env.insert("RUFA_GENERATION".to_string(), generation.to_string());

    Ok(env)
}

fn resolve_reference(
    reference: &crate::config::model::EnvReference,
    assignments: &HashMap<String, HashMap<String, u16>>,
    state: &RuntimeState,
) -> Result<String> {
    match reference.resource {
        ReferenceResource::Port { ref name } => {
            if let Some(ports) = assignments.get(&reference.target) {
                if let Some(value) = ports.get(name) {
                    return Ok(value.to_string());
                }
            }

            if let Some(state) = state.targets.get(&reference.target) {
                if let Some(value) = state.ports.get(name) {
                    return Ok(value.to_string());
                }
            }

            bail!(
                "reference to {target}:{port} could not be resolved",
                target = reference.target,
                port = name
            );
        }
    }
}

fn resolve_command(unit: &UnitTarget, runtime_watch: bool) -> Result<String> {
    match unit.driver_type.to_ascii_lowercase().as_str() {
        "java-spring-boot" => resolve_java_spring_boot_command(unit, runtime_watch),
        "bun" => resolve_bun_command(unit, runtime_watch),
        "bash" => resolve_script_property(unit, "command"),
        _ => resolve_generic_command(unit),
    }
}

fn resolve_java_spring_boot_command(unit: &UnitTarget, runtime_watch: bool) -> Result<String> {
    if let Some(command) = unit
        .properties
        .get("command")
        .and_then(|value| value.as_str())
    {
        return Ok(
            adjust_runtime_watch_flag(command, runtime_watch, " --spring-boot-devtools")
                .to_string(),
        );
    }

    if let Some(module) = unit
        .properties
        .get("module")
        .and_then(|value| value.as_str())
    {
        // Default to Maven Spring Boot plugin.
        let mut jvm_args =
            "-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:${PORT_DEBUG}"
                .to_string();
        if runtime_watch {
            jvm_args.push(' ');
            jvm_args.push_str("-Dspring.devtools.restart.enabled=true");
        }
        let command = format!(
            "mvn -pl {module} spring-boot:run -Dspring-boot.run.jvmArguments=\"{args}\"",
            module = module,
            args = jvm_args
        );
        return Ok(command);
    }

    if let Some(jar) = unit.properties.get("jar").and_then(|value| value.as_str()) {
        return Ok(format!("java -jar {}", jar));
    }

    bail!(
        "target {} (java-spring-boot) must define `command`, `module`, or `jar` property",
        unit.name
    );
}

fn resolve_script_property(unit: &UnitTarget, key: &str) -> Result<String> {
    unit.properties
        .get(key)
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .ok_or_else(|| {
            anyhow!(
                "target {} requires `{}` property in configuration",
                unit.name,
                key
            )
        })
}

fn resolve_generic_command(unit: &UnitTarget) -> Result<String> {
    if let Some(command) = unit
        .properties
        .get("command")
        .and_then(|value| value.as_str())
    {
        return Ok(command.to_string());
    }

    if let Some(script) = unit
        .properties
        .get("script")
        .and_then(|value| value.as_str())
    {
        return Ok(script.to_string());
    }

    bail!(
        "target {} is missing a `command` or `script` property",
        unit.name
    );
}

fn resolve_bun_command(unit: &UnitTarget, runtime_watch: bool) -> Result<String> {
    let command = resolve_script_property(unit, "script")?;
    if runtime_watch && !command.contains("--watch") {
        Ok(format!("{command} --watch"))
    } else {
        Ok(command)
    }
}

fn adjust_runtime_watch_flag(command: &str, runtime_watch: bool, flag: &str) -> String {
    if runtime_watch && !command.contains(flag.trim()) {
        format!("{command}{flag}")
    } else {
        command.to_string()
    }
}

#[derive(Debug, Clone, Copy)]
enum RestartSchedule {
    Scheduled { attempt: u32, delay: Duration },
    AlreadyPending,
    Skipped,
}

async fn schedule_restart_core(
    strategy: RestartStrategy,
    target: &str,
    attempt_override: Option<u32>,
    restart_state: &Arc<Mutex<HashMap<String, BackoffStatus>>>,
    restart_suppressed: &Arc<Mutex<HashSet<String>>>,
    restart_tx: &mpsc::Sender<RestartJob>,
) -> Result<RestartSchedule, ()> {
    if !matches!(strategy, RestartStrategy::Backoff) {
        return Ok(RestartSchedule::Skipped);
    }

    if consume_suppression_shared(restart_suppressed, target).await {
        reset_backoff_state_shared(restart_state, target).await;
        return Ok(RestartSchedule::Skipped);
    }

    let mut statuses = restart_state.lock().await;
    let status = statuses.entry(target.to_string()).or_default();
    if status.pending {
        if let Some(value) = attempt_override {
            if value > status.attempt {
                status.attempt = value;
            }
        }
        return Ok(RestartSchedule::AlreadyPending);
    }
    let next_attempt = attempt_override.unwrap_or_else(|| status.attempt.saturating_add(1).max(1));
    status.attempt = next_attempt;
    status.pending = true;
    drop(statuses);

    let delay = compute_backoff_delay(next_attempt);
    tracing::warn!(
        target,
        attempt = next_attempt,
        delay_secs = delay.as_secs_f64(),
        "scheduling service restart with backoff"
    );
    if let Err(error) = restart_tx
        .send(RestartJob {
            target: target.to_string(),
            attempt: next_attempt,
            delay,
        })
        .await
    {
        tracing::error!(%error, target, "failed to enqueue restart job");
        let mut statuses = restart_state.lock().await;
        if let Some(entry) = statuses.get_mut(target) {
            entry.pending = false;
        }
        return Err(());
    }

    Ok(RestartSchedule::Scheduled {
        attempt: next_attempt,
        delay,
    })
}

async fn clear_pending_flag_shared(
    restart_state: &Arc<Mutex<HashMap<String, BackoffStatus>>>,
    target: &str,
) {
    let mut statuses = restart_state.lock().await;
    if let Some(status) = statuses.get_mut(target) {
        status.pending = false;
    }
}

async fn reset_backoff_state_shared(
    restart_state: &Arc<Mutex<HashMap<String, BackoffStatus>>>,
    target: &str,
) {
    let mut statuses = restart_state.lock().await;
    let entry = statuses.entry(target.to_string()).or_default();
    entry.attempt = 0;
    entry.pending = false;
}

async fn consume_suppression_shared(
    restart_suppressed: &Arc<Mutex<HashSet<String>>>,
    target: &str,
) -> bool {
    let mut suppressed = restart_suppressed.lock().await;
    suppressed.remove(target)
}

async fn mark_suppressed_shared(restart_suppressed: &Arc<Mutex<HashSet<String>>>, target: &str) {
    let mut suppressed = restart_suppressed.lock().await;
    suppressed.insert(target.to_string());
}

fn compute_backoff_delay(attempt: u32) -> Duration {
    let clamped = attempt.max(1).min(8);
    let seconds = 1u64 << (clamped - 1);
    Duration::from_secs(seconds.min(60))
}

async fn update_runtime_restart_state(
    state: &Arc<RwLock<RuntimeState>>,
    target: &str,
    pending: bool,
    attempt: u32,
    delay: Option<Duration>,
    scheduled_for: Option<SystemTime>,
) {
    let mut guard = state.write().await;
    if let Some(entry) = guard.targets.get_mut(target) {
        entry.restart_pending = pending;
        entry.restart_attempt = attempt;
        entry.restart_delay = delay;
        entry.restart_scheduled_for = scheduled_for;
    }
}

fn signal_process(pid: u32, signal: Signal) -> Result<()> {
    match send_unix_signal(Pid::from_raw(pid as i32), signal) {
        Ok(()) => Ok(()),
        Err(err) if err == Errno::ESRCH => Ok(()),
        Err(err) => Err(anyhow!(
            "failed to send {:?} to pid {}: {}",
            signal,
            pid,
            err
        )),
    }
}

#[derive(Debug)]
struct ProcessHandle {
    exit_notify: Notify,
    exited: AtomicBool,
}

impl ProcessHandle {
    fn new() -> Self {
        Self {
            exit_notify: Notify::new(),
            exited: AtomicBool::new(false),
        }
    }

    async fn wait_for_exit(&self) {
        if self.exited.load(Ordering::SeqCst) {
            return;
        }
        self.exit_notify.notified().await;
    }

    fn signal_exit(&self) {
        if !self.exited.swap(true, Ordering::SeqCst) {
            self.exit_notify.notify_waiters();
        }
    }
}

#[derive(Debug, Default)]
struct BackoffStatus {
    attempt: u32,
    pending: bool,
}

#[derive(Debug)]
struct RestartJob {
    target: String,
    attempt: u32,
    delay: Duration,
}

#[derive(Debug)]
struct PortSentry {
    port: u16,
    shutdown: tokio::sync::watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl PortSentry {
    async fn bind(port: u16) -> io::Result<Self> {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
        let listener = TokioTcpListener::bind(addr).await?;
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            let mut shutdown_rx_inner = shutdown_rx.clone();
            loop {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_ok() && *shutdown_rx.borrow() {
                            break;
                        }
                        let _ = shutdown_rx.borrow_and_update();
                    }
                    result = listener.accept() => {
                        match result {
                            Ok((stream, _)) => {
                                let hold = sleep(Duration::from_millis(100));
                                tokio::pin!(hold);
                                tokio::select! {
                                    _ = &mut hold => {}
                                    changed = shutdown_rx_inner.changed() => {
                                        if changed.is_ok() && *shutdown_rx_inner.borrow() {
                                            return;
                                        }
                                        let _ = shutdown_rx_inner.borrow_and_update();
                                        continue;
                                    }
                                }
                                drop(stream);
                            }
                            Err(err) => {
                                if err.kind() == io::ErrorKind::Interrupted {
                                    continue;
                                }
                                if err.kind() == io::ErrorKind::WouldBlock {
                                    yield_now().await;
                                    continue;
                                }
                                tracing::debug!(%err, port, "port sentinel accept error");
                                sleep(Duration::from_millis(50)).await;
                            }
                        }
                    }
                }
            }
        });
        Ok(Self {
            port,
            shutdown: shutdown_tx,
            handle,
        })
    }

    async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.handle.await;
    }
}

#[derive(Debug)]
struct PortSentrySet {
    sentries: Vec<PortSentry>,
}

impl PortSentrySet {
    async fn bind_from_ports(ports: &HashMap<String, u16>) -> io::Result<Self> {
        let mut seen = HashSet::new();
        let mut sentries = Vec::new();
        for port in ports.values().copied() {
            if seen.insert(port) {
                sentries.push(PortSentry::bind(port).await?);
            }
        }
        Ok(Self { sentries })
    }

    async fn stop(self) {
        for sentry in self.sentries {
            sentry.stop().await;
        }
    }
}

#[derive(Debug, Default)]
struct PortAllocator {
    reserved: HashSet<u16>,
}

impl PortAllocator {
    fn allocate(&mut self, selection: crate::config::model::PortSelection) -> Result<u16> {
        match selection {
            crate::config::model::PortSelection::Auto => self.allocate_ephemeral(),
            crate::config::model::PortSelection::AutoRange { start, end } => {
                self.allocate_from_range(start, end)
            }
            crate::config::model::PortSelection::Fixed(port) => {
                self.allocate_fixed(port)?;
                Ok(port)
            }
        }
    }

    fn allocate_ephemeral(&mut self) -> Result<u16> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .context("binding to ephemeral port for allocation")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        if self.reserved.insert(port) {
            Ok(port)
        } else {
            self.allocate_ephemeral()
        }
    }

    fn allocate_from_range(&mut self, start: u16, end: u16) -> Result<u16> {
        for port in start..=end {
            if self.reserved.contains(&port) {
                continue;
            }
            if TcpListener::bind(("127.0.0.1", port)).is_ok() {
                self.reserved.insert(port);
                return Ok(port);
            }
        }
        bail!("no free ports in range {start}-{end}");
    }

    fn allocate_fixed(&mut self, port: u16) -> Result<()> {
        if self.reserved.contains(&port) {
            bail!("port {port} is already reserved");
        }

        TcpListener::bind(("127.0.0.1", port))
            .map(|listener| drop(listener))
            .with_context(|| format!("port {port} is not available"))?;
        self.reserved.insert(port);
        Ok(())
    }

    fn release_ports<I>(&mut self, ports: I)
    where
        I: Iterator<Item = u16>,
    {
        for port in ports {
            self.reserved.remove(&port);
        }
    }
}

async fn restore_port_sentries(
    store: Arc<RwLock<HashMap<String, PortSentrySet>>>,
    target: String,
    ports: HashMap<String, u16>,
) -> Result<()> {
    let sentries = PortSentrySet::bind_from_ports(&ports)
        .await
        .with_context(|| format!("binding placeholder listeners for target {}", target))?;
    let previous = {
        let mut map = store.write().await;
        map.insert(target.clone(), sentries)
    };
    if let Some(prev) = previous {
        prev.stop().await;
    }
    Ok(())
}

pub(crate) fn load_env_overrides(path: &Path) -> Result<HashMap<String, String>> {
    parse_lenient_dotenv(path).map_err(|error| {
        anyhow!(
            "failed to parse environment overrides from {:?}: {error}",
            path
        )
    })
}

fn parse_lenient_dotenv(path: &Path) -> io::Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    let contents = fs::read_to_string(path)?;

    for (idx, raw) in contents.lines().enumerate() {
        let mut line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        line = line.strip_prefix("export ").unwrap_or(line).trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some(eq_index) = line.find('=') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("no '=' on line {}", idx + 1),
            ));
        };

        let (key_part, value_part_with_eq) = line.split_at(eq_index);
        let key = key_part.trim();
        if key.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("empty key on line {}", idx + 1),
            ));
        }

        let mut value = value_part_with_eq[1..].trim().to_string();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = value[1..value.len() - 1].to_string();
            value = value
                .replace(r"\n", "\n")
                .replace(r"\t", "\t")
                .replace(r"\\", "\\")
                .replace(r#"\""#, "\"")
                .replace(r"\'", "'");
        } else if let Some(hash) = value.find('#') {
            value = value[..hash].trim_end().to_string();
        }

        map.insert(key.to_string(), value);
    }

    Ok(map)
}

fn exit_message(status: &std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        format!("exited with code {code}")
    } else if let Some(signal) = exit_signal(status) {
        format!("terminated by signal {signal}")
    } else {
        "process exited".to_string()
    }
}

fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    }

    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::load_env_overrides;
    use anyhow::Result;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn load_env_overrides_accepts_dot_names() -> Result<()> {
        let mut file = NamedTempFile::new()?;
        writeln!(file, "APP.CONFIG=value")?;
        writeln!(file, "feature.flag=true")?;
        writeln!(file, ".leading.dot=present")?;
        writeln!(file, "service-name=web")?;
        writeln!(file, "QUOTED=\"line\\nvalue\"")?;
        writeln!(file, "WITH_COMMENT=value # trailing info")?;
        file.flush()?;

        let overrides = load_env_overrides(file.path())?;
        assert_eq!(
            overrides.get("APP.CONFIG").map(String::as_str),
            Some("value")
        );
        assert_eq!(
            overrides.get("feature.flag").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            overrides.get(".leading.dot").map(String::as_str),
            Some("present")
        );
        assert_eq!(
            overrides.get("service-name").map(String::as_str),
            Some("web")
        );
        assert_eq!(
            overrides.get("QUOTED").map(String::as_str),
            Some("line\nvalue")
        );
        assert_eq!(
            overrides.get("WITH_COMMENT").map(String::as_str),
            Some("value")
        );
        Ok(())
    }
}

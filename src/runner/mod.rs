#![allow(dead_code)]

//! Runtime process launcher and supervisor.

mod drivers;
mod ports;
mod restart;

use std::{
    collections::{HashMap, HashSet},
    fs,
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
use once_cell::sync::Lazy;
use regex::Regex;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{Mutex, Notify, RwLock, mpsc},
    time::{sleep, timeout},
};

use crate::{
    config::{
        self, Config, EnvValue, ReferenceResource, RestartConfig, Target, TargetBehavior,
        TargetName, UnitTarget, WatchPreference,
    },
    debug_update::DebugUpdateContext,
    env,
    logging::{EventLogger, LogEvent, LogStream},
    state::{ExitStatus, RuntimeState, TargetState},
};

use drivers::resolve_command;
use ports::{PortAllocator, PortSentrySet, restore_port_sentries};
use restart::{RestartCoordinator, RestartJob, RestartSchedule, update_runtime_restart_state};

#[derive(Debug)]
pub struct Runner {
    config_path: PathBuf,
    logger: Arc<EventLogger>,
    state: Arc<RwLock<RuntimeState>>,
    port_allocator: Arc<Mutex<PortAllocator>>,
    processes: Arc<RwLock<HashMap<String, Arc<ProcessHandle>>>>,
    port_sentries: Arc<RwLock<HashMap<String, PortSentrySet>>>,
    restart: Arc<RestartCoordinator>,
    restart_rx: Mutex<mpsc::Receiver<RestartJob>>,
    history_keep: usize,
    env_override_path: Option<PathBuf>,
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
        env_override_path: Option<PathBuf>,
    ) -> Self {
        let (restart_tx, restart_rx) = mpsc::channel(32);
        let restart_state = Arc::new(Mutex::new(HashMap::new()));
        let restart_suppressed = Arc::new(Mutex::new(HashSet::new()));
        let restart = Arc::new(RestartCoordinator::new(
            restart_config.strategy,
            restart_state,
            restart_suppressed,
            restart_tx.clone(),
        ));
        Self {
            config_path: config_path.into(),
            logger,
            state,
            port_allocator: Arc::new(Mutex::new(PortAllocator::default())),
            processes: Arc::new(RwLock::new(HashMap::new())),
            port_sentries: Arc::new(RwLock::new(HashMap::new())),
            restart,
            restart_rx: Mutex::new(restart_rx),
            history_keep: history_keep.max(1),
            env_override_path,
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
        let outcome = self.restart.schedule(target, attempt_override).await?;

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
        self.restart.clear_pending(target).await;
    }

    async fn reset_backoff_state(&self, target: &str) {
        self.restart.reset_backoff(target).await;
    }

    async fn consume_suppression(&self, target: &str) -> bool {
        self.restart.consume_suppression(target).await
    }

    async fn mark_restart_suppressed(&self, target: &str) {
        self.restart.mark_suppressed(target).await;
    }

    async fn on_target_launched(&self, target: &str, behavior: &TargetBehavior) {
        let _ = self.restart.consume_suppression(target).await;
        if matches!(behavior, TargetBehavior::Service) {
            self.restart.reset_backoff(target).await;
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

        self.watch_spec_from_config(&config, targets)
    }

    fn watch_spec_from_config(&self, config: &Config, targets: &[String]) -> Result<WatchSpec> {
        let base = self
            .config_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let base = fs::canonicalize(&base).unwrap_or(base);

        let units = resolve_units(config, targets)?;
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

        self.launch_units(&config, units, watch).await
    }

    async fn launch_units(
        &self,
        config: &Config,
        units: Vec<UnitTarget>,
        watch: bool,
    ) -> Result<LaunchOutcome> {
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

        let mut env_file_vars = HashMap::new();
        if let Some(path) = config.env.read_env_file.as_ref() {
            if !path.exists() {
                tracing::warn!(
                    path = %path.display(),
                    "environment file not found; continuing without overrides"
                );
            } else {
                match env::load_env_overrides(path) {
                    Ok(vars) => env_file_vars.extend(vars),
                    Err(error) => {
                        tracing::warn!(%error, "failed to load environment overrides");
                    }
                }
            }
        }
        if let Some(path) = self.env_override_path.as_ref() {
            if !path.exists() {
                tracing::warn!(
                    path = %path.display(),
                    "runtime env override file not found; continuing without overrides"
                );
            } else {
                match env::load_env_overrides(path) {
                    Ok(vars) => env_file_vars.extend(vars),
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            path = %path.display(),
                            "failed to load runtime environment overrides"
                        );
                    }
                }
            }
        }

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
            let runtime_watch_allowed = matches!(unit.behavior, TargetBehavior::Service);
            let wants_runtime_watch = if runtime_watch_allowed {
                if watch {
                    matches!(
                        unit.watch_preference,
                        WatchPreference::PreferRuntimeSupplied
                    )
                } else {
                    previous.map(|state| state.runtime_watch).unwrap_or(false)
                }
            } else {
                false
            };
            let wants_rufa_watch = if matches!(unit.behavior, TargetBehavior::Service) {
                if watch {
                    matches!(unit.watch_preference, WatchPreference::Rufa)
                } else {
                    previous.map(|state| state.rufa_watch).unwrap_or(false)
                }
            } else {
                false
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
            Some(self.watch_spec_from_config(config, &rufa_watch_targets)?)
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
        let restart = self.restart.clone();
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
                    match restart.schedule(&target_name, None).await {
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
                    restart.reset_backoff(&target_name).await;
                    update_runtime_restart_state(&state, &target_name, false, 0, None, None).await;
                }
            }

            if let Err(error) = ports::restore_port_sentries(
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

        self.launch_units(&config, units, watch_requested).await
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

    let interpolated = env
        .into_iter()
        .map(|(key, value)| {
            let rendered = interpolate_placeholders(&value, assignments, state)?;
            Ok((key, rendered))
        })
        .collect::<Result<HashMap<_, _>>>()?;

    Ok(interpolated)
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

fn interpolate_placeholders(
    input: &str,
    assignments: &HashMap<String, HashMap<String, u16>>,
    state: &RuntimeState,
) -> Result<String> {
    static PLACEHOLDER: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"\$\{([A-Za-z0-9_.:-]+)\}|\{([A-Za-z0-9_.:-]+)\}")
            .expect("valid placeholder regex")
    });

    let mut rendered = String::with_capacity(input.len());
    let mut last = 0usize;
    for captures in PLACEHOLDER.captures_iter(input) {
        let m = captures.get(0).expect("match present");
        rendered.push_str(&input[last..m.start()]);
        let spec = captures
            .get(1)
            .or_else(|| captures.get(2))
            .unwrap()
            .as_str();
        if let Some(value) = resolve_port_placeholder(spec, assignments, state)? {
            rendered.push_str(&value);
        } else {
            rendered.push_str(m.as_str());
        }
        last = m.end();
    }
    rendered.push_str(&input[last..]);
    Ok(rendered)
}

fn resolve_port_placeholder(
    spec: &str,
    assignments: &HashMap<String, HashMap<String, u16>>,
    state: &RuntimeState,
) -> Result<Option<String>> {
    let Some((target, remainder)) = spec.split_once(':') else {
        return Ok(None);
    };

    if let Some(port_name) = remainder.strip_prefix("port.") {
        let reference = crate::config::model::EnvReference {
            target: target.to_string(),
            resource: ReferenceResource::Port {
                name: port_name.to_string(),
            },
        };
        return resolve_reference(&reference, assignments, state)
            .map(Some)
            .or_else(|_| Ok(None));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_port_placeholders_in_env_values() -> Result<()> {
        let mut assignments: HashMap<String, HashMap<String, u16>> = HashMap::new();
        assignments.insert(
            "infra".to_string(),
            HashMap::from([("mysql".to_string(), 3307), ("mq".to_string(), 5673)]),
        );

        let mut state = RuntimeState::default();
        state
            .targets
            .entry("infra".to_string())
            .or_default()
            .ports
            .insert("analytics".to_string(), 9999);

        let value = "mysql://127.0.0.1:${infra:port.mysql}?alt={infra:port.analytics}";
        let rendered = interpolate_placeholders(value, &assignments, &state)?;
        assert_eq!(rendered, "mysql://127.0.0.1:3307?alt=9999");

        Ok(())
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

#![allow(dead_code)]

//! Runtime process launcher and supervisor.

mod control;
mod drivers;
mod generation;
mod ports;

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
    sync::{Mutex, Notify, RwLock},
    time::{sleep, timeout},
};

use crate::{
    config::{
        self, Config, EnvValue, ReferenceResource, RefreshWatchType, Target, TargetBehavior,
        TargetName, UnitTarget,
    },
    debug_update::DebugUpdateContext,
    env,
    logging::{EventLogger, LogEvent, LogStream},
    state::{ExitStatus, RuntimeState, TargetState},
};

use control::{Action, DesiredState, TargetControl, TargetKind, TargetStatus};
use drivers::resolve_command;
use generation::TargetGenerationStore;
use ports::{PortAllocator, PortSentrySet, restore_port_sentries};

#[derive(Debug)]
pub struct Runner {
    config_path: PathBuf,
    logger: Arc<EventLogger>,
    state: Arc<RwLock<RuntimeState>>,
    refresh_on_change: Arc<AtomicBool>,
    port_allocator: Arc<Mutex<PortAllocator>>,
    processes: Arc<RwLock<HashMap<String, Arc<ProcessHandle>>>>,
    port_sentries: Arc<RwLock<HashMap<String, PortSentrySet>>>,
    controls: Arc<RwLock<HashMap<String, TargetControl>>>,
    in_flight: Arc<Mutex<HashSet<String>>>,
    generations: Arc<Mutex<TargetGenerationStore>>,
    history_keep: usize,
    env_override_path: Option<PathBuf>,
}

#[derive(Debug, Default, Clone)]
pub struct WatchSpec {
    pub services: HashMap<String, Vec<PathBuf>>,
    pub stability: Duration,
}

#[derive(Debug, Default)]
pub struct StartOutcome {
    pub rufa_watch: Option<WatchSpec>,
}

#[derive(Debug, Clone)]
pub struct ControlInfo {
    pub desired_state: String,
    pub action_plan: String,
}

async fn mutate_control<F>(
    controls: &Arc<RwLock<HashMap<String, TargetControl>>>,
    target: &str,
    update: F,
) where
    F: FnOnce(&mut TargetControl),
{
    let mut guard = controls.write().await;
    let entry = guard
        .entry(target.to_string())
        .or_insert_with(TargetControl::new);
    update(entry);
}

impl Runner {
    pub fn new(
        config_path: impl Into<PathBuf>,
        logger: Arc<EventLogger>,
        state: Arc<RwLock<RuntimeState>>,
        refresh_on_change: Arc<AtomicBool>,
        history_keep: usize,
        env_override_path: Option<PathBuf>,
    ) -> Result<Self> {
        let generations =
            TargetGenerationStore::new().with_context(|| "initializing target generation store")?;
        Ok(Self {
            config_path: config_path.into(),
            logger,
            state,
            refresh_on_change,
            port_allocator: Arc::new(Mutex::new(PortAllocator::default())),
            processes: Arc::new(RwLock::new(HashMap::new())),
            port_sentries: Arc::new(RwLock::new(HashMap::new())),
            controls: Arc::new(RwLock::new(HashMap::new())),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            generations: Arc::new(Mutex::new(generations)),
            history_keep: history_keep.max(1),
            env_override_path,
        })
    }

    pub fn spawn_background_tasks(self: &Arc<Self>) {
        let runner = Arc::clone(self);
        tokio::spawn(async move {
            runner.control_loop().await;
        });
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    async fn next_generation(&self, target: &str) -> Result<u64> {
        let mut store = self.generations.lock().await;
        store
            .next(target)
            .with_context(|| format!("updating generation for target {target}"))
    }

    pub async fn generation_snapshot(&self) -> Vec<(String, u64)> {
        let store = self.generations.lock().await;
        store.all()
    }

    async fn ensure_control_entry(&self, target: &str) {
        let mut guard = self.controls.write().await;
        guard
            .entry(target.to_string())
            .or_insert_with(TargetControl::new);
    }

    async fn set_desired_state_for(&self, target: &str, desired: DesiredState) {
        let mut guard = self.controls.write().await;
        let entry = guard
            .entry(target.to_string())
            .or_insert_with(TargetControl::new);
        entry.request_desired_state(desired);
    }

    async fn update_desired_state_only(&self, target: &str, desired: DesiredState) {
        let mut guard = self.controls.write().await;
        let entry = guard
            .entry(target.to_string())
            .or_insert_with(TargetControl::new);
        entry.desired_state = desired;
    }

    async fn mark_target_running(&self, target: &str) {
        let mut guard = self.controls.write().await;
        let entry = guard
            .entry(target.to_string())
            .or_insert_with(TargetControl::new);
        entry.settle_desired_state(DesiredState::Running);
    }

    async fn mark_target_stopped(&self, target: &str) {
        let mut guard = self.controls.write().await;
        let entry = guard
            .entry(target.to_string())
            .or_insert_with(TargetControl::new);
        entry.settle_desired_state(DesiredState::Stopped);
    }

    async fn replace_action_plan(&self, target: &str, plan: Action) {
        let mut guard = self.controls.write().await;
        let entry = guard
            .entry(target.to_string())
            .or_insert_with(TargetControl::new);
        entry.action_plan = plan;
    }

    async fn set_target_kind(&self, target: &str, kind: TargetKind) {
        let mut guard = self.controls.write().await;
        let entry = guard
            .entry(target.to_string())
            .or_insert_with(TargetControl::new);
        entry.set_kind(kind);
    }

    async fn schedule_plan_rebuild(&self, target: &str) {
        let mut guard = self.controls.write().await;
        let entry = guard
            .entry(target.to_string())
            .or_insert_with(TargetControl::new);
        entry.schedule_plan_rebuild();
    }

    async fn control_snapshot(&self) -> HashMap<String, TargetControl> {
        let guard = self.controls.read().await;
        guard.clone()
    }

    pub async fn control_info(&self, target: &str) -> Option<ControlInfo> {
        let guard = self.controls.read().await;
        guard.get(target).map(|entry| ControlInfo {
            desired_state: entry.desired_state.to_string(),
            action_plan: entry.action_plan.describe(),
        })
    }

    async fn target_status(&self, target: &str) -> TargetStatus {
        let processes = self.processes.read().await;
        TargetStatus {
            is_running: processes.contains_key(target),
        }
    }

    async fn control_loop(self: Arc<Self>) {
        loop {
            let targets: Vec<String> = {
                let guard = self.controls.read().await;
                if guard.is_empty() {
                    drop(guard);
                    sleep(Duration::from_millis(250)).await;
                    continue;
                }
                guard.keys().cloned().collect()
            };

            for target in targets {
                let status = self.target_status(&target).await;
                let action_to_run = {
                    let mut guard = self.controls.write().await;
                    let entry = guard
                        .entry(target.clone())
                        .or_insert_with(TargetControl::new);
                    if !entry.desired_state.is_fulfilled(&status) && entry.action_plan.is_idle() {
                        entry.action_plan = entry.desired_state.make_plan(&target);
                    }

                    match &entry.action_plan {
                        Action::Idle(_) => None,
                        action => Some(action.clone()),
                    }
                };

                if let Some(action) = action_to_run {
                    let mut in_flight = self.in_flight.lock().await;
                    if !in_flight.insert(target.clone()) {
                        drop(in_flight);
                        continue;
                    }
                    drop(in_flight);

                    self.execute_action(target.clone(), action).await;

                    let mut in_flight = self.in_flight.lock().await;
                    in_flight.remove(&target);
                }
            }

            sleep(Duration::from_millis(150)).await;
        }
    }

    async fn execute_action(&self, target: String, action: Action) {
        match action {
            Action::Start {
                on_success,
                on_failure,
            } => match self.start_targets(&[target.clone()]).await {
                Ok(_) => {
                    self.replace_action_plan(&target, *on_success).await;
                }
                Err(error) => {
                    tracing::error!(%error, target = %target, "start action failed");
                    self.replace_action_plan(&target, *on_failure).await;
                }
            },
            Action::Stop {
                on_success,
                on_failure,
            } => match self.terminate_target(&target).await {
                Ok(_) => {
                    self.replace_action_plan(&target, *on_success).await;
                }
                Err(error) => {
                    tracing::error!(%error, target = %target, "stop action failed");
                    self.replace_action_plan(&target, *on_failure).await;
                }
            },
            Action::SetDesiredState { desired, next } => {
                self.update_desired_state_only(&target, desired).await;
                self.replace_action_plan(&target, *next).await;
            }
            Action::Idle(result) => {
                tracing::trace!(
                    target = %target,
                    result = %result,
                    "skipping idle action"
                );
                self.replace_action_plan(&target, Action::Idle(result))
                    .await;
            }
        }
    }

    pub fn logger(&self) -> Arc<EventLogger> {
        self.logger.clone()
    }

    pub fn state(&self) -> Arc<RwLock<RuntimeState>> {
        self.state.clone()
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

    pub async fn request_start(&self, targets: &[String]) -> Result<StartOutcome> {
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

        let mut rufa_watch_targets = Vec::new();
        for unit in &units {
            let name = unit.name.clone();
            self.set_desired_state_for(&name, DesiredState::Running)
                .await;
            let kind = match &unit.behavior {
                TargetBehavior::Service => TargetKind::Service,
                TargetBehavior::Job { .. } => TargetKind::Job,
            };
            self.set_target_kind(&name, kind).await;

            if matches!(&unit.behavior, TargetBehavior::Service) {
                if matches!(unit.refresh_watch_type, RefreshWatchType::Rufa) {
                    rufa_watch_targets.push(name);
                }
            }
        }

        let rufa_watch = if rufa_watch_targets.is_empty() {
            None
        } else {
            Some(self.watch_spec_from_config(&config, &rufa_watch_targets)?)
        };

        Ok(StartOutcome { rufa_watch })
    }

    pub async fn request_stop_all(&self) -> Result<()> {
        let targets = {
            let processes = self.processes.read().await;
            processes.keys().cloned().collect::<Vec<_>>()
        };

        for target in targets {
            self.set_desired_state_for(&target, DesiredState::Stopped)
                .await;
        }

        Ok(())
    }

    pub async fn request_restart(&self, targets: &[String], all: bool) -> Result<StartOutcome> {
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

        let mut rufa_watch_targets = Vec::new();
        for unit in &units {
            let name = unit.name.clone();
            self.set_desired_state_for(&name, DesiredState::Restarted)
                .await;
            let kind = match &unit.behavior {
                TargetBehavior::Service => TargetKind::Service,
                TargetBehavior::Job { .. } => TargetKind::Job,
            };
            self.set_target_kind(&name, kind).await;

            if matches!(&unit.behavior, TargetBehavior::Service)
                && matches!(unit.refresh_watch_type, RefreshWatchType::Rufa)
            {
                rufa_watch_targets.push(name);
            }
        }

        let rufa_watch = if rufa_watch_targets.is_empty() {
            None
        } else {
            Some(self.watch_spec_from_config(&config, &rufa_watch_targets)?)
        };

        Ok(StartOutcome { rufa_watch })
    }

    pub async fn restart_stale_targets(&self) -> Result<(Vec<String>, StartOutcome)> {
        let stale = {
            let mut state_guard = self.state.write().await;
            state_guard
                .targets
                .iter_mut()
                .filter_map(|(name, entry)| {
                    if entry.change_noticed_restart_necessary && entry.pid.is_some() {
                        entry.change_noticed_restart_necessary = false;
                        Some(name.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };

        if stale.is_empty() {
            return Ok((Vec::new(), StartOutcome::default()));
        }

        let outcome = self.request_restart(&stale, false).await?;
        Ok((stale, outcome))
    }

    pub async fn restart_runtime_refresh_targets(&self) -> Result<(Vec<String>, StartOutcome)> {
        let running_targets = {
            let processes = self.processes.read().await;
            processes.keys().cloned().collect::<Vec<_>>()
        };

        if running_targets.is_empty() {
            return Ok((Vec::new(), StartOutcome::default()));
        }

        let config = config::load_from_path(&self.config_path).with_context(|| {
            format!(
                "loading configuration from {:?}",
                self.config_path.as_os_str()
            )
        })?;

        let runtime_targets: Vec<String> = running_targets
            .into_iter()
            .filter(|name| {
                matches!(
                    config.targets.get(name),
                    Some(Target::Unit(unit))
                        if matches!(unit.behavior, TargetBehavior::Service)
                            && matches!(
                                unit.refresh_watch_type,
                                RefreshWatchType::PreferRuntimeSupplied
                            )
                )
            })
            .collect();

        if runtime_targets.is_empty() {
            return Ok((Vec::new(), StartOutcome::default()));
        }

        let units = resolve_units(&config, &runtime_targets)?;
        let mut rufa_watch_targets = Vec::new();
        for unit in &units {
            let name = unit.name.clone();
            self.set_desired_state_for(&name, DesiredState::Restarted)
                .await;
            let kind = match &unit.behavior {
                TargetBehavior::Service => TargetKind::Service,
                TargetBehavior::Job { .. } => TargetKind::Job,
            };
            self.set_target_kind(&name, kind).await;

            if matches!(&unit.behavior, TargetBehavior::Service)
                && matches!(unit.refresh_watch_type, RefreshWatchType::Rufa)
            {
                rufa_watch_targets.push(name);
            }
        }

        let rufa_watch = if rufa_watch_targets.is_empty() {
            None
        } else {
            Some(self.watch_spec_from_config(&config, &rufa_watch_targets)?)
        };

        Ok((runtime_targets, StartOutcome { rufa_watch }))
    }

    pub async fn request_restart_from_watch(&self, target: &str) -> bool {
        tracing::debug!(target = %target, "evaluating watch-triggered restart");
        let should_consider = {
            let guard = self.controls.read().await;
            guard
                .get(target)
                .map(|control| {
                    control.kind == TargetKind::Service
                        && matches!(control.desired_state, DesiredState::Running)
                })
                .unwrap_or(false)
        };
        if !should_consider {
            tracing::debug!(target = %target, "watch restart skipped (target not running service)");
            return false;
        }

        let refresh_enabled = self.refresh_on_change.load(Ordering::SeqCst);

        let (runtime_watch, rufa_watch) = {
            let mut state_guard = self.state.write().await;
            let Some(entry) = state_guard.targets.get_mut(target) else {
                tracing::debug!(target = %target, "watch restart skipped (target state missing)");
                return false;
            };
            if !entry.rufa_watch {
                tracing::debug!(target = %target, "watch restart skipped (target not tracked by rufa watch)");
                return false;
            }
            if !refresh_enabled {
                entry.change_noticed_restart_necessary = true;
                tracing::debug!(target = %target, "watch restart deferred (refresh disabled; marked stale)");
                return false;
            }
            entry.change_noticed_restart_necessary = false;
            (entry.runtime_watch, entry.rufa_watch)
        };

        if runtime_watch || !rufa_watch {
            tracing::debug!(
                target = %target,
                runtime_watch,
                rufa_watch,
                "watch restart skipped (runtime-managed or not eligible)"
            );
            return false;
        }

        let mut guard = self.controls.write().await;
        if let Some(control) = guard.get_mut(target) {
            if control.kind == TargetKind::Service
                && matches!(control.desired_state, DesiredState::Running)
            {
                control.request_desired_state(DesiredState::Restarted);
                tracing::debug!(target = %target, "watch restart requested");
                return true;
            }
        }
        tracing::debug!(target = %target, "watch restart skipped (control state unavailable)");
        false
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

    pub async fn start_targets(&self, targets: &[String]) -> Result<StartOutcome> {
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

        self.start_units(&config, units).await
    }

    async fn start_units(&self, config: &Config, units: Vec<UnitTarget>) -> Result<StartOutcome> {
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

        let state_snapshot = {
            let guard = self.state.read().await;
            guard.clone()
        };
        let assignments = self.allocate_ports(&units, &state_snapshot).await?;

        let mut rufa_watch_targets = Vec::new();

        let refresh_on_change = self.refresh_on_change.load(Ordering::SeqCst);

        for unit in units {
            let ports = assignments.get(&unit.name).cloned().unwrap_or_default();

            let generation = self.next_generation(&unit.name).await?;

            let is_service = matches!(unit.behavior, TargetBehavior::Service);
            let wants_runtime_watch = is_service
                && matches!(
                    unit.refresh_watch_type,
                    RefreshWatchType::PreferRuntimeSupplied
                )
                && refresh_on_change;
            let wants_rufa_watch =
                is_service && matches!(unit.refresh_watch_type, RefreshWatchType::Rufa);

            tracing::info!(
                target = %unit.name,
                behavior = ?unit.behavior,
                refresh_watch_type = ?unit.refresh_watch_type,
                refresh_on_change,
                wants_runtime_watch,
                wants_rufa_watch,
                watch_paths = ?unit.watch_paths,
                "evaluating target for watch configuration"
            );

            if wants_rufa_watch {
                rufa_watch_targets.push(unit.name.clone());
                tracing::info!(target = %unit.name, "target will use rufa-managed watching");
            } else if is_service {
                tracing::info!(
                    target = %unit.name,
                    "service target will not use rufa-managed watching"
                );
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
            tracing::info!("start request produced no rufa-managed watch targets");
            None
        } else {
            let spec = self.watch_spec_from_config(config, &rufa_watch_targets)?;
            let mut service_dirs: HashMap<String, Vec<String>> = HashMap::new();
            for (service, paths) in &spec.services {
                service_dirs.insert(
                    service.clone(),
                    paths
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect(),
                );
            }
            tracing::info!(
                services = ?service_dirs,
                stability_secs = spec.stability.as_secs_f64(),
                "start request resolved rufa watch specification"
            );
            Some(spec)
        };

        Ok(StartOutcome { rufa_watch })
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
        previous: &RuntimeState,
    ) -> Result<HashMap<TargetName, HashMap<String, u16>>> {
        let mut allocator = self.port_allocator.lock().await;
        let mut assignments = HashMap::new();
        for unit in units {
            let mut ports = HashMap::new();
            let previous_ports = previous
                .targets
                .get(&unit.name)
                .map(|state| state.ports.clone())
                .unwrap_or_default();
            for (name, definition) in &unit.ports {
                let mut assigned = None;
                if let Some(prev) = previous_ports.get(name) {
                    if definition.selection.permits(*prev) {
                        match allocator.reserve_specific(*prev) {
                            Ok(()) => {
                                assigned = Some(*prev);
                            }
                            Err(error) => {
                                tracing::debug!(
                                    target = %unit.name,
                                    port_name = %name,
                                    port = *prev,
                                    %error,
                                    "previous port unavailable; allocating a new one"
                                );
                            }
                        }
                    }
                }

                let port = if let Some(port) = assigned {
                    port
                } else {
                    allocator
                        .allocate(definition.selection)
                        .with_context(|| format!("allocating port for {}.{}", unit.name, name))?
                };
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
        }

        let behavior = unit.behavior.clone();
        self.mark_target_running(&target_name).await;

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
        let controls = self.controls.clone();
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

            let mut unexpected_service_exit = false;
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
                    }

                    if matches!(behavior, TargetBehavior::Service) && !exit_status.success() {
                        unexpected_service_exit = true;
                    }
                }
                Err(error) => {
                    tracing::error!(%error, target = %target_name, "child wait failed");
                    let mut state_guard = state.write().await;
                    if let Some(entry) = state_guard.targets.get_mut(&target_name) {
                        entry.pid = None;
                    }
                    if matches!(behavior, TargetBehavior::Service) {
                        unexpected_service_exit = true;
                    }
                }
            }

            if unexpected_service_exit {
                tracing::warn!(
                    target = %target_name,
                    "service exited unexpectedly; desired state remains {:?}",
                    {
                        let guard = controls.read().await;
                        guard
                            .get(&target_name)
                            .map(|entry| entry.desired_state)
                    }
                );
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

    pub async fn stop_targets(&self, targets: &[String]) -> Result<()> {
        for target in targets {
            self.set_desired_state_for(target, DesiredState::Stopped)
                .await;
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
                entry.change_noticed_restart_necessary = false;
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

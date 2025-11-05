//! File watching and restart orchestration.
//!
//! The current implementation watches the workspace root and triggers
//! restarts for service targets that were launched with `--watch`. Jobs are
//! intentionally excluded from automatic restarts.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::{
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use tracing::{error, info, warn};

use crate::runner::{Runner, WatchSpec};

#[derive(Debug, Default)]
pub struct WatchManager {
    inner: Mutex<Option<WatchSession>>,
}

impl WatchManager {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// Enable watch mode for the provided targets. Only service targets will
    /// be restarted when changes are detected.
    pub async fn enable_watch(&self, runner: Arc<Runner>, spec: WatchSpec) -> Result<()> {
        if spec.services.is_empty() {
            info!("watch mode requested but no services found");
            self.disable().await;
            return Ok(());
        }

        let stability = spec.stability;
        let root = runner
            .config_path()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let services_arc = Arc::new(spec.services);
        let (tx, mut rx) = mpsc::channel::<notify::Result<Event>>(128);

        let mut watcher = notify::recommended_watcher({
            let tx = tx.clone();
            move |res| {
                if tx.blocking_send(res).is_err() {
                    warn!("file watcher channel closed; dropping event");
                }
            }
        })?;

        watcher.watch(&root, RecursiveMode::Recursive)?;

        let runner_weak = Arc::downgrade(&runner);
        let services_for_task = services_arc.clone();
        let pending_targets = Arc::new(Mutex::new(HashSet::new()));
        let task = tokio::spawn(async move {
            let debounce = stability;
            let pending = Arc::new(AtomicBool::new(false));

            while let Some(event) = rx.recv().await {
                match event {
                    Ok(event) => {
                        let matches = matched_targets(&event, services_for_task.as_ref());
                        if matches.is_empty() {
                            continue;
                        }

                        let Some(runner) = runner_weak.upgrade() else {
                            break;
                        };

                        {
                            let mut guard = pending_targets.lock().await;
                            for name in matches {
                                guard.insert(name);
                            }
                        }

                        if pending.swap(true, Ordering::SeqCst) {
                            continue;
                        }

                        let runner_clone = runner.clone();
                        let pending_flag = pending.clone();
                        let pending_set = pending_targets.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(debounce).await;
                            let targets: Vec<String> = {
                                let mut guard = pending_set.lock().await;
                                let items = guard.iter().cloned().collect();
                                guard.clear();
                                items
                            };

                            if !targets.is_empty() {
                                if let Err(error) =
                                    runner_clone.restart_targets(&targets, false).await
                                {
                                    error!(%error, "watch-triggered restart failed");
                                }
                            }
                            pending_flag.store(false, Ordering::SeqCst);
                        });
                    }
                    Err(error) => {
                        error!(%error, "file watcher error");
                    }
                }
            }
        });

        let mut inner = self.inner.lock().await;
        if let Some(previous) = inner.take() {
            drop(previous);
        }
        *inner = Some(WatchSession {
            services: services_arc,
            _watcher: watcher,
            task,
        });

        if let Some(session) = inner.as_ref() {
            let names: Vec<&String> = session.services.keys().collect();
            info!(
                root = %root.display(),
                services = ?names,
                stability_secs = stability.as_secs_f64(),
                "watch mode enabled"
            );
        }

        Ok(())
    }

    /// Disable watch mode and release any active watchers.
    pub async fn disable(&self) {
        let mut inner = self.inner.lock().await;
        if inner.take().is_some() {
            info!("watch mode disabled");
        }
    }
}

struct WatchSession {
    services: Arc<HashMap<String, Vec<PathBuf>>>,
    _watcher: RecommendedWatcher,
    task: JoinHandle<()>,
}

impl std::fmt::Debug for WatchSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let services: Vec<&String> = self.services.keys().collect();
        f.debug_struct("WatchSession")
            .field("services", &services)
            .finish_non_exhaustive()
    }
}

impl Drop for WatchSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn should_ignore_path(path: &Path) -> bool {
    for component in path.components() {
        let Some(name) = component.as_os_str().to_str() else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        if matches!(lower.as_str(), ".git" | "target" | ".rufa") {
            return true;
        }
    }

    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let lower = name.to_ascii_lowercase();
        if lower == "rufa.log"
            || lower.starts_with("rufa.log.")
            || lower.ends_with("~")
            || lower.ends_with(".swp")
            || lower.ends_with(".tmp")
        {
            return true;
        }
    }

    false
}

fn matched_targets(event: &Event, services: &HashMap<String, Vec<PathBuf>>) -> Vec<String> {
    if !matches!(
        event.kind,
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) | EventKind::Any
    ) {
        return Vec::new();
    }

    if event.paths.is_empty() {
        return services.keys().cloned().collect();
    }

    let mut matched = HashSet::new();
    for path in &event.paths {
        if should_ignore_path(path) {
            continue;
        }

        for (target, dirs) in services {
            if dirs.iter().any(|dir| path.starts_with(dir)) {
                matched.insert(target.clone());
            }
        }
    }

    matched.into_iter().collect()
}

use anyhow::Result;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, mpsc};

use crate::config::model::RestartStrategy;

#[derive(Debug)]
pub(crate) struct RestartJob {
    pub(crate) target: String,
    pub(crate) attempt: u32,
    pub(crate) delay: Duration,
}

#[derive(Debug)]
pub(crate) struct RestartCoordinator {
    strategy: RestartStrategy,
    state: Arc<Mutex<HashMap<String, BackoffStatus>>>,
    suppressed: Arc<Mutex<HashSet<String>>>,
    tx: mpsc::Sender<RestartJob>,
}

impl RestartCoordinator {
    pub(crate) fn new(
        strategy: RestartStrategy,
        state: Arc<Mutex<HashMap<String, BackoffStatus>>>,
        suppressed: Arc<Mutex<HashSet<String>>>,
        tx: mpsc::Sender<RestartJob>,
    ) -> Self {
        Self {
            strategy,
            state,
            suppressed,
            tx,
        }
    }

    pub(crate) fn sender(&self) -> mpsc::Sender<RestartJob> {
        self.tx.clone()
    }

    pub(crate) fn state(&self) -> Arc<Mutex<HashMap<String, BackoffStatus>>> {
        Arc::clone(&self.state)
    }

    pub(crate) fn suppressed(&self) -> Arc<Mutex<HashSet<String>>> {
        Arc::clone(&self.suppressed)
    }

    pub(crate) async fn schedule(
        &self,
        target: &str,
        attempt_override: Option<u32>,
    ) -> Result<RestartSchedule, ()> {
        schedule_restart_core(
            self.strategy,
            target,
            attempt_override,
            &self.state,
            &self.suppressed,
            &self.tx,
        )
        .await
    }

    pub(crate) async fn clear_pending(&self, target: &str) {
        clear_pending_flag_shared(&self.state, target).await;
    }

    pub(crate) async fn reset_backoff(&self, target: &str) {
        reset_backoff_state_shared(&self.state, target).await;
    }

    pub(crate) async fn consume_suppression(&self, target: &str) -> bool {
        consume_suppression_shared(&self.suppressed, target).await
    }

    pub(crate) async fn mark_suppressed(&self, target: &str) {
        mark_suppressed_shared(&self.suppressed, target).await;
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum RestartSchedule {
    Scheduled { attempt: u32, delay: Duration },
    AlreadyPending,
    Skipped,
}

#[derive(Debug, Default)]
pub(crate) struct BackoffStatus {
    pub(crate) attempt: u32,
    pub(crate) pending: bool,
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

pub(crate) async fn update_runtime_restart_state(
    state: &Arc<tokio::sync::RwLock<crate::state::RuntimeState>>,
    target: &str,
    pending: bool,
    attempt: u32,
    delay: Option<Duration>,
    scheduled_for: Option<std::time::SystemTime>,
) {
    let mut guard = state.write().await;
    if let Some(entry) = guard.targets.get_mut(target) {
        entry.restart_pending = pending;
        entry.restart_attempt = attempt;
        entry.restart_delay = delay;
        entry.restart_scheduled_for = scheduled_for;
    }
}

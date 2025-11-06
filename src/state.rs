#![allow(dead_code)]

//! In-memory state tracking for the supervisor.

use std::collections::{HashMap, VecDeque};
use std::time::SystemTime;

#[derive(Debug, Default, Clone)]
pub struct RuntimeState {
    pub targets: HashMap<String, TargetState>,
}

#[derive(Debug, Clone)]
pub struct TargetState {
    /// Monotonically increasing generation count.
    pub generation: u64,
    /// Process identifier for the running child, if available.
    pub pid: Option<u32>,
    /// Assigned ports keyed by logical name (http, debug, etc).
    pub ports: HashMap<String, u16>,
    /// Most recent log line captured for this target.
    pub last_log_line: Option<String>,
    /// Exit information from the last completed process run.
    pub last_exit: Option<ExitStatus>,
    /// Whether runtime-provided watch mode is active for this target.
    pub runtime_watch: bool,
    /// Whether rufa-managed watch mode is active for this target.
    pub rufa_watch: bool,
    /// Whether a change was detected while refresh-on-change was disabled.
    pub change_noticed_restart_necessary: bool,
    /// Historical run records (most recent first).
    pub history: VecDeque<RunRecord>,
}

#[derive(Debug, Clone)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub when: SystemTime,
}

#[derive(Debug, Clone)]
pub struct RunRecord {
    pub generation: u64,
    pub last_log_line: Option<String>,
    pub exit: Option<ExitStatus>,
}

impl Default for TargetState {
    fn default() -> Self {
        Self {
            generation: 0,
            pid: None,
            ports: HashMap::new(),
            last_log_line: None,
            last_exit: None,
            runtime_watch: false,
            rufa_watch: false,
            change_noticed_restart_necessary: false,
            history: VecDeque::new(),
        }
    }
}

impl TargetState {
    pub fn record_snapshot(&mut self, keep: usize) {
        if self.generation == 0 {
            return;
        }

        let record = RunRecord {
            generation: self.generation,
            last_log_line: self.last_log_line.clone(),
            exit: self.last_exit.clone(),
        };
        self.history.push_front(record);
        while self.history.len() > keep {
            self.history.pop_back();
        }
    }

    pub fn reset_for_new_run(
        &mut self,
        generation: u64,
        ports: HashMap<String, u16>,
        runtime_watch: bool,
        rufa_watch: bool,
    ) {
        self.generation = generation;
        self.pid = None;
        self.ports = ports;
        self.last_log_line = None;
        self.last_exit = None;
        self.runtime_watch = runtime_watch;
        self.rufa_watch = rufa_watch;
        self.change_noticed_restart_necessary = false;
    }
}

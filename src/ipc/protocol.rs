use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientCommand {
    Ping,
    Run(RunRequest),
    Kill(KillRequest),
    Configure(ConfigureRequest),
    Info(InfoRequest),
    Log(LogRequest),
    Restart(RestartRequest),
    Stop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRequest {
    pub targets: Vec<String>,
    pub watch: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillRequest {
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigureRequest {
    pub default_watch: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfoRequest {
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRequest {
    pub targets: Vec<String>,
    pub tail: Option<usize>,
    pub follow: bool,
    pub generation: Option<u64>,
    pub kind: LogRequestKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LogRequestKind {
    Combined,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartRequest {
    pub targets: Vec<String>,
    pub all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerResponse {
    Ack,
    Error(String),
    Info(InfoResponse),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfoResponse {
    pub running: Vec<RunningTargetSummary>,
    pub stopped: Vec<StoppedTargetSummary>,
    pub available: Vec<AvailableTargetSummary>,
    pub generations: Vec<TargetGenerationSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningTargetSummary {
    pub name: String,
    pub kind: BehaviorKind,
    pub config: TargetConfigSummary,
    pub current: TargetRunState,
    pub history: Vec<RunHistorySummary>,
    pub control: Option<ControlStateSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoppedTargetSummary {
    pub name: String,
    pub kind: BehaviorKind,
    pub config: TargetConfigSummary,
    pub last: Option<RunHistorySummary>,
    pub history: Vec<RunHistorySummary>,
    pub control: Option<ControlStateSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableTargetSummary {
    pub name: String,
    pub kind: BehaviorKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetConfigSummary {
    pub driver: String,
    pub watch: WatchPreferenceKind,
    pub watch_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum WatchPreferenceKind {
    Rufa,
    Runtime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetRunState {
    pub generation: u64,
    pub pid: Option<u32>,
    pub ports: Vec<PortSummary>,
    pub last_log: Option<String>,
    pub last_exit: Option<ExitDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortSummary {
    pub name: String,
    pub port: u16,
    pub protocol: Option<String>,
    pub is_debug: bool,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunHistorySummary {
    pub generation: u64,
    pub last_log: Option<String>,
    pub exit: Option<ExitDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlStateSummary {
    pub desired_state: String,
    pub action_plan: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetGenerationSummary {
    pub name: String,
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitDetails {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub at: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum BehaviorKind {
    Service,
    Job,
}

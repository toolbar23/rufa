#![allow(dead_code)]

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use serde::Serialize;
use toml::Value;

pub type TargetName = String;

#[derive(Debug, Clone)]
pub struct Config {
    pub log: LogConfig,
    pub restart: RestartConfig,
    pub env: EnvConfig,
    pub watch: WatchConfig,
    pub run_history: RunHistoryConfig,
    pub debug_update: Option<DebugUpdateConfig>,
    pub targets: BTreeMap<TargetName, Target>,
}

#[derive(Debug, Clone)]
pub struct LogConfig {
    pub file: PathBuf,
    pub rotation_after: Option<Duration>,
    pub rotation_size_bytes: Option<u64>,
    pub keep_history_count: usize,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            file: PathBuf::from("rufa.log"),
            rotation_after: None,
            rotation_size_bytes: None,
            keep_history_count: 5,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RestartConfig {
    pub strategy: RestartStrategy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartStrategy {
    No,
    Backoff,
}

impl Default for RestartConfig {
    fn default() -> Self {
        Self {
            strategy: RestartStrategy::Backoff,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct EnvConfig {
    pub read_env_file: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct WatchConfig {
    pub stability: Duration,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            stability: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunHistoryConfig {
    pub keep_runs: usize,
}

impl Default for RunHistoryConfig {
    fn default() -> Self {
        Self { keep_runs: 3 }
    }
}

#[derive(Debug, Clone)]
pub struct DebugUpdateConfig {
    pub prefix: String,
    pub vscode: Option<PathBuf>,
    pub zed: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum Target {
    Composite(CompositeTarget),
    Unit(UnitTarget),
}

impl Target {
    pub fn is_composite(&self) -> bool {
        matches!(self, Target::Composite(_))
    }
}

#[derive(Debug, Clone)]
pub struct CompositeTarget {
    pub name: TargetName,
    pub members: Vec<TargetName>,
    pub properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct UnitTarget {
    pub name: TargetName,
    pub driver_type: String,
    pub behavior: TargetBehavior,
    pub env: BTreeMap<String, EnvValue>,
    pub ports: BTreeMap<String, PortDefinition>,
    pub properties: BTreeMap<String, Value>,
    pub watch_paths: Vec<PathBuf>,
    pub watch_preference: WatchPreference,
}

#[derive(Debug, Clone)]
pub enum TargetBehavior {
    Service,
    Job { timeout: Option<Duration> },
}

impl TargetBehavior {
    pub fn is_service(&self) -> bool {
        matches!(self, TargetBehavior::Service)
    }

    pub fn is_job(&self) -> bool {
        matches!(self, TargetBehavior::Job { .. })
    }
}

#[derive(Debug, Clone)]
pub enum EnvValue {
    Literal(String),
    Reference(EnvReference),
}

impl EnvValue {
    pub fn as_literal(&self) -> Option<&str> {
        match self {
            EnvValue::Literal(s) => Some(s),
            EnvValue::Reference(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnvReference {
    pub target: TargetName,
    #[serde(flatten)]
    pub resource: ReferenceResource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type")]
pub enum ReferenceResource {
    #[serde(rename = "port")]
    Port { name: String },
}

#[derive(Debug, Clone)]
pub struct PortDefinition {
    pub protocol: PortProtocol,
    pub selection: PortSelection,
    #[allow(dead_code)]
    pub properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortProtocol {
    Http,
    Ftp,
    Tcp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortSelection {
    Auto,
    AutoRange { start: u16, end: u16 },
    Fixed(u16),
}

impl PortSelection {
    pub fn as_range(&self) -> (u16, u16) {
        match self {
            PortSelection::Auto => (0, 0),
            PortSelection::AutoRange { start, end } => (*start, *end),
            PortSelection::Fixed(value) => (*value, *value),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchPreference {
    Rufa,
    PreferRuntimeSupplied,
}

impl Default for WatchPreference {
    fn default() -> Self {
        WatchPreference::PreferRuntimeSupplied
    }
}

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};
use toml::Value;

#[derive(Debug, Default, Deserialize)]
pub struct RawConfig {
    #[serde(default)]
    pub log: RawLogConfig,
    #[serde(default)]
    pub restart: RawRestartConfig,
    #[serde(default)]
    pub env: Option<RawEnvConfig>,
    #[serde(default, rename = "debug-update")]
    pub debug_update: Option<RawDebugUpdateConfig>,
    #[serde(default)]
    pub watch: Option<RawWatchConfig>,
    #[serde(default, rename = "run_history")]
    pub run_history: Option<RawRunHistoryConfig>,
    #[serde(default, rename = "target")]
    pub targets: BTreeMap<String, RawTarget>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RawLogConfig {
    pub file: Option<String>,
    pub rotation_after_seconds: Option<u64>,
    pub rotation_after_size_mb: Option<u64>,
    pub keep_history_count: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RawRestartConfig {
    #[serde(rename = "type")]
    pub strategy: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RawEnvConfig {
    pub read_env_file: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RawWatchConfig {
    pub stability_seconds: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RawRunHistoryConfig {
    pub keep_runs: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RawDebugUpdateConfig {
    pub prefix: Option<String>,
    #[serde(default)]
    pub update: Option<RawDebugUpdateTargets>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RawDebugUpdateTargets {
    pub vscode: Option<String>,
    pub zed: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RawTarget {
    #[serde(rename = "type")]
    pub target_type: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub targets: TargetList,
    #[serde(default)]
    pub env: BTreeMap<String, Value>,
    #[serde(default)]
    pub port: BTreeMap<String, RawPortConfig>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub watch: TargetList,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RawPortConfig {
    #[serde(rename = "type")]
    pub port_type: Option<String>,
    #[serde(default)]
    pub auto: Option<bool>,
    #[serde(default)]
    pub auto_range: Option<Value>,
    #[serde(default)]
    pub fixed: Option<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Default)]
pub struct TargetList(pub Vec<String>);

impl TargetList {
    pub fn into_vec(self) -> Vec<String> {
        self.0
    }
}

impl<'de> Deserialize<'de> for TargetList {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = TargetList;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a list of target names or a comma-separated string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let items = value
                    .split(',')
                    .map(|item| item.trim())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect();
                Ok(TargetList(items))
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element::<String>()? {
                    if !item.trim().is_empty() {
                        items.push(item);
                    }
                }
                Ok(TargetList(items))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(TargetList(Vec::new()))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

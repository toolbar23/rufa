use std::{io, path::PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file '{path}': {source}")]
    ReadFailure {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("missing '{field}' for target '{target}'")]
    MissingField { target: String, field: &'static str },
    #[error("unknown target kind '{value}' for target '{target}'")]
    UnknownTargetKind { target: String, value: String },
    #[error("composite target '{target}' requires at least one member")]
    EmptyComposite { target: String },
    #[error("missing '{field}' for port '{port}' on target '{target}'")]
    MissingPortField {
        target: String,
        port: String,
        field: &'static str,
    },
    #[error("unknown port type '{value}' for '{target}:{port}'")]
    UnknownPortType {
        target: String,
        port: String,
        value: String,
    },
    #[error(
        "port selection for '{target}:{port}' must specify exactly one of auto/auto_range/fixed"
    )]
    InvalidPortSelection { target: String, port: String },
    #[error("port definition '{value}' for '{target}:{port}' is invalid")]
    InvalidPortSpec {
        target: String,
        port: String,
        value: String,
    },
    #[error("port values must be within valid u16 range for '{target}:{port}'")]
    PortOutOfRange { target: String, port: String },
    #[error("environment reference '{value}' for '{target}:{key}' is invalid")]
    InvalidEnvReference {
        target: String,
        key: String,
        value: String,
    },
    #[error("unknown refresh watch type '{value}' for target '{target}'")]
    UnknownRefreshWatchType { target: String, value: String },
}

pub type ConfigResult<T> = Result<T, ConfigError>;

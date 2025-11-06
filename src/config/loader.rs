use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use super::{
    error::{ConfigError, ConfigResult},
    model::{
        CompositeTarget, Config, DebugUpdateConfig, EnvConfig, EnvReference, EnvValue, LogConfig,
        PortDefinition, PortProtocol, PortSelection, ReferenceResource, RefreshWatchType,
        RunHistoryConfig, Target, TargetBehavior, TargetName, UnitTarget, WatchConfig,
    },
    raw::{RawConfig, RawRunHistoryConfig, RawTarget, RawWatchConfig},
};

const MB_IN_BYTES: u64 = 1024 * 1024;

pub fn load_from_path<P: AsRef<Path>>(path: P) -> ConfigResult<Config> {
    let path_ref = path.as_ref();
    let raw_contents = fs::read_to_string(path_ref).map_err(|source| ConfigError::ReadFailure {
        path: path_ref.to_path_buf(),
        source,
    })?;
    load_from_str(path_ref, &raw_contents)
}

pub fn load_from_str(config_path: &Path, contents: &str) -> ConfigResult<Config> {
    let raw: RawConfig = toml::from_str(contents)?;
    convert_raw_config(config_path, raw)
}

fn convert_raw_config(config_path: &Path, raw: RawConfig) -> ConfigResult<Config> {
    let log = convert_log_config(config_path, raw.log);
    let env = convert_env_config(config_path, raw.env);
    let watch = convert_watch_config(raw.watch);
    let run_history = convert_run_history_config(raw.run_history);
    let debug_update = convert_debug_update_config(config_path, raw.debug_update)?;
    let targets = convert_targets(config_path, raw.targets)?;

    Ok(Config {
        log,
        env,
        watch,
        run_history,
        debug_update,
        targets,
    })
}

fn convert_log_config(config_path: &Path, raw: super::raw::RawLogConfig) -> LogConfig {
    let file = raw
        .file
        .map(|value| resolve_relative_path(config_path, value))
        .unwrap_or_else(|| PathBuf::from("rufa.log"));

    let rotation_after = raw.rotation_after_seconds.map(Duration::from_secs);

    let rotation_size_bytes = raw
        .rotation_after_size_mb
        .map(|mb| mb.saturating_mul(MB_IN_BYTES));

    let keep_history_count = raw.keep_history_count.unwrap_or(5);

    LogConfig {
        file,
        rotation_after,
        rotation_size_bytes,
        keep_history_count,
    }
}

fn convert_env_config(config_path: &Path, raw: Option<super::raw::RawEnvConfig>) -> EnvConfig {
    let mut env = EnvConfig::default();
    if let Some(raw_env) = raw {
        if let Some(read_env_file) = raw_env.read_env_file {
            env.read_env_file = Some(resolve_relative_path(config_path, read_env_file));
        }
    }
    env
}

fn convert_watch_config(raw: Option<RawWatchConfig>) -> WatchConfig {
    let mut config = WatchConfig::default();
    if let Some(raw_watch) = raw {
        if let Some(seconds) = raw_watch.stability_seconds {
            config.stability = Duration::from_secs(seconds.max(1));
        }
    }
    config
}

fn convert_run_history_config(raw: Option<RawRunHistoryConfig>) -> RunHistoryConfig {
    let mut config = RunHistoryConfig::default();
    if let Some(raw_history) = raw {
        if let Some(keep) = raw_history.keep_runs {
            config.keep_runs = keep.max(1);
        }
    }
    config
}

fn convert_debug_update_config(
    config_path: &Path,
    raw: Option<super::raw::RawDebugUpdateConfig>,
) -> ConfigResult<Option<DebugUpdateConfig>> {
    let Some(raw_config) = raw else {
        return Ok(None);
    };

    let prefix = raw_config.prefix.unwrap_or_else(|| "RUFA".to_string());
    let update = raw_config.update.unwrap_or_default();

    let vscode = update.vscode.as_ref().and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(resolve_relative_path(config_path, trimmed.to_string()))
        }
    });
    let zed = update.zed.as_ref().and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(resolve_relative_path(config_path, trimmed.to_string()))
        }
    });

    if vscode.is_none() && zed.is_none() {
        return Ok(None);
    }

    Ok(Some(DebugUpdateConfig {
        prefix,
        vscode,
        zed,
    }))
}

fn convert_targets(
    config_path: &Path,
    raw_targets: BTreeMap<TargetName, RawTarget>,
) -> ConfigResult<BTreeMap<TargetName, Target>> {
    let mut targets = BTreeMap::new();
    for (name, raw) in raw_targets {
        let target = convert_target(config_path, name.clone(), raw)?;
        targets.insert(name, target);
    }
    Ok(targets)
}

fn convert_target(config_path: &Path, name: TargetName, raw: RawTarget) -> ConfigResult<Target> {
    let RawTarget {
        target_type,
        kind,
        targets,
        env,
        port,
        timeout_seconds,
        watch,
        extra,
    } = raw;

    let target_type = target_type.ok_or_else(|| ConfigError::MissingField {
        target: name.clone(),
        field: "type",
    })?;

    if target_type.eq_ignore_ascii_case("composite") {
        let members = targets.into_vec();
        if members.is_empty() {
            return Err(ConfigError::EmptyComposite { target: name });
        }
        let _ = watch.into_vec();
        return Ok(Target::Composite(CompositeTarget {
            name,
            members,
            properties: extra,
        }));
    }

    let behavior = determine_behavior(&name, kind.as_deref(), timeout_seconds)?;
    let env = convert_env_values(&name, env)?;
    let ports = convert_ports(&name, port)?;
    let watch_paths = watch
        .into_vec()
        .into_iter()
        .map(|entry| resolve_relative_path(config_path, entry))
        .collect();
    let refresh_watch_type = extract_refresh_watch_type(&name, &extra)?;
    let mut properties = extra;
    properties.remove("refresh_watch_type");
    Ok(Target::Unit(UnitTarget {
        name,
        driver_type: target_type,
        behavior,
        env,
        ports,
        properties,
        watch_paths,
        refresh_watch_type,
    }))
}

fn determine_behavior(
    target_name: &str,
    kind: Option<&str>,
    timeout_seconds: Option<u64>,
) -> ConfigResult<TargetBehavior> {
    let default_kind = "service";
    let value = kind.unwrap_or(default_kind).to_ascii_lowercase();
    match value.as_str() {
        "service" => Ok(TargetBehavior::Service),
        "job" => Ok(TargetBehavior::Job {
            timeout: timeout_seconds.map(|secs| Duration::from_secs(secs)),
        }),
        other => Err(ConfigError::UnknownTargetKind {
            target: target_name.to_string(),
            value: other.to_string(),
        }),
    }
}

fn convert_env_values(
    target_name: &str,
    map: BTreeMap<String, toml::Value>,
) -> ConfigResult<BTreeMap<String, EnvValue>> {
    let mut result = BTreeMap::new();
    for (key, value) in map {
        let env_value = match value {
            toml::Value::String(s) => parse_env_string(target_name, &key, s)?,
            toml::Value::Integer(i) => EnvValue::Literal(i.to_string()),
            toml::Value::Float(f) => EnvValue::Literal(f.to_string()),
            toml::Value::Boolean(b) => EnvValue::Literal(b.to_string()),
            toml::Value::Datetime(dt) => EnvValue::Literal(dt.to_string()),
            other => EnvValue::Literal(other.to_string()),
        };
        result.insert(key, env_value);
    }
    Ok(result)
}

fn extract_refresh_watch_type(
    target_name: &str,
    properties: &BTreeMap<String, toml::Value>,
) -> ConfigResult<RefreshWatchType> {
    let Some(value) = properties.get("refresh_watch_type") else {
        return Ok(RefreshWatchType::default());
    };
    let Some(text) = value.as_str() else {
        return Err(ConfigError::UnknownRefreshWatchType {
            target: target_name.to_string(),
            value: value.to_string(),
        });
    };

    match text.to_ascii_uppercase().as_str() {
        "RUFA" | "RUNFA" => Ok(RefreshWatchType::Rufa),
        "PREFER_RUNTIME_SUPPLIED" | "RUNTIME" | "RUNTIME_SUPPLIED" => {
            Ok(RefreshWatchType::PreferRuntimeSupplied)
        }
        _ => Err(ConfigError::UnknownRefreshWatchType {
            target: target_name.to_string(),
            value: text.to_string(),
        }),
    }
}

fn parse_env_string(target_name: &str, key: &str, value: String) -> ConfigResult<EnvValue> {
    if let Some(reference) = parse_reference(&value) {
        return Ok(EnvValue::Reference(reference));
    }

    let trimmed = value.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Err(ConfigError::InvalidEnvReference {
            target: target_name.to_string(),
            key: key.to_string(),
            value,
        });
    }

    Ok(EnvValue::Literal(value))
}

fn parse_reference(value: &str) -> Option<EnvReference> {
    let trimmed = value.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        let inner = trimmed.trim_start_matches('{').trim_end_matches('}');
        let mut parts = inner.split(':');
        let target = parts.next()?.trim();
        let resource = parts.next()?.trim();

        if resource.starts_with("port.") {
            let port_name = resource.trim_start_matches("port.").to_string();
            return Some(EnvReference {
                target: target.to_string(),
                resource: ReferenceResource::Port { name: port_name },
            });
        }
    }
    None
}

fn convert_ports(
    target_name: &str,
    ports: BTreeMap<String, super::raw::RawPortConfig>,
) -> ConfigResult<BTreeMap<String, PortDefinition>> {
    let mut result = BTreeMap::new();
    for (port_name, raw) in ports {
        let definition = convert_port(target_name, &port_name, raw)?;
        result.insert(port_name, definition);
    }
    Ok(result)
}

fn convert_port(
    target_name: &str,
    port_name: &str,
    raw: super::raw::RawPortConfig,
) -> ConfigResult<PortDefinition> {
    let super::raw::RawPortConfig {
        port_type,
        auto,
        auto_range,
        fixed,
        extra,
    } = raw;

    let protocol = parse_port_protocol(target_name, port_name, port_type)?;
    let selection = determine_port_selection(target_name, port_name, auto, auto_range, fixed)?;

    Ok(PortDefinition {
        protocol,
        selection,
        properties: extra,
    })
}

fn parse_port_protocol(
    target_name: &str,
    port_name: &str,
    value: Option<String>,
) -> ConfigResult<PortProtocol> {
    let value = value.ok_or_else(|| ConfigError::MissingPortField {
        target: target_name.to_string(),
        port: port_name.to_string(),
        field: "type",
    })?;

    match value.to_ascii_lowercase().as_str() {
        "http" => Ok(PortProtocol::Http),
        "ftp" => Ok(PortProtocol::Ftp),
        "tcp" => Ok(PortProtocol::Tcp),
        other => Err(ConfigError::UnknownPortType {
            target: target_name.to_string(),
            port: port_name.to_string(),
            value: other.to_string(),
        }),
    }
}

fn determine_port_selection(
    target_name: &str,
    port_name: &str,
    auto: Option<bool>,
    auto_range: Option<toml::Value>,
    fixed: Option<toml::Value>,
) -> ConfigResult<PortSelection> {
    let mut selection: Option<PortSelection> = None;

    if auto.unwrap_or(false) {
        merge_selection(&mut selection, PortSelection::Auto, target_name, port_name)?;
    }

    if let Some(value) = auto_range {
        let (start, end) = parse_port_range_value(target_name, port_name, &value)?;
        merge_selection(
            &mut selection,
            PortSelection::AutoRange { start, end },
            target_name,
            port_name,
        )?;
    }

    if let Some(value) = fixed {
        let port = parse_port_number_value(target_name, port_name, &value)?;
        merge_selection(
            &mut selection,
            PortSelection::Fixed(port),
            target_name,
            port_name,
        )?;
    }

    selection.ok_or_else(|| ConfigError::InvalidPortSelection {
        target: target_name.to_string(),
        port: port_name.to_string(),
    })
}

fn merge_selection(
    current: &mut Option<PortSelection>,
    next: PortSelection,
    target_name: &str,
    port_name: &str,
) -> ConfigResult<()> {
    if current.is_some() {
        return Err(ConfigError::InvalidPortSelection {
            target: target_name.to_string(),
            port: port_name.to_string(),
        });
    }
    *current = Some(next);
    Ok(())
}

fn parse_port_range_value(
    target_name: &str,
    port_name: &str,
    value: &toml::Value,
) -> ConfigResult<(u16, u16)> {
    match value {
        toml::Value::String(s) => parse_port_range_string(target_name, port_name, s),
        toml::Value::Array(arr) if arr.len() == 2 => {
            let start = parse_port_number_value(target_name, port_name, &arr[0])?;
            let end = parse_port_number_value(target_name, port_name, &arr[1])?;
            validate_port_range(target_name, port_name, start, end)?;
            Ok((start, end))
        }
        other => Err(ConfigError::InvalidPortSpec {
            target: target_name.to_string(),
            port: port_name.to_string(),
            value: other.to_string(),
        }),
    }
}

fn parse_port_range_string(
    target_name: &str,
    port_name: &str,
    value: &str,
) -> ConfigResult<(u16, u16)> {
    let trimmed = value.trim();
    let (start, end) = trimmed
        .split_once('-')
        .ok_or_else(|| ConfigError::InvalidPortSpec {
            target: target_name.to_string(),
            port: port_name.to_string(),
            value: trimmed.to_string(),
        })?;

    let start = start
        .trim()
        .parse::<u16>()
        .map_err(|_| ConfigError::InvalidPortSpec {
            target: target_name.to_string(),
            port: port_name.to_string(),
            value: trimmed.to_string(),
        })?;
    let end = end
        .trim()
        .parse::<u16>()
        .map_err(|_| ConfigError::InvalidPortSpec {
            target: target_name.to_string(),
            port: port_name.to_string(),
            value: trimmed.to_string(),
        })?;

    validate_port_range(target_name, port_name, start, end)?;
    Ok((start, end))
}

fn parse_port_number_value(
    target_name: &str,
    port_name: &str,
    value: &toml::Value,
) -> ConfigResult<u16> {
    match value {
        toml::Value::Integer(i) => u16::try_from(*i).map_err(|_| ConfigError::PortOutOfRange {
            target: target_name.to_string(),
            port: port_name.to_string(),
        }),
        toml::Value::String(s) => {
            s.trim()
                .parse::<u16>()
                .map_err(|_| ConfigError::InvalidPortSpec {
                    target: target_name.to_string(),
                    port: port_name.to_string(),
                    value: s.to_string(),
                })
        }
        other => Err(ConfigError::InvalidPortSpec {
            target: target_name.to_string(),
            port: port_name.to_string(),
            value: other.to_string(),
        }),
    }
}

fn validate_port_range(
    target_name: &str,
    port_name: &str,
    start: u16,
    end: u16,
) -> ConfigResult<()> {
    if start > end {
        return Err(ConfigError::InvalidPortSpec {
            target: target_name.to_string(),
            port: port_name.to_string(),
            value: format!("{start}-{end}"),
        });
    }
    Ok(())
}

fn resolve_relative_path(base: &Path, value: String) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        return path;
    }

    let base_dir = if base.is_dir() {
        base.to_path_buf()
    } else {
        base.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    };
    base_dir.join(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        path::{Path, PathBuf},
        time::Duration,
    };

    use tempfile::NamedTempFile;

    fn fixture_path() -> &'static Path {
        Path::new("/tmp/rufa/rufa.toml")
    }

    #[test]
    fn parses_sample_configuration() {
        let toml = r#"
[debug-update]
prefix = "RUFA"
update.vscode = ".vscode/launch.json"
update.zed = ".zed/debug.json"

[log]
file = "rufa.log"
rotation_after_seconds = 86400
rotation_after_size_mb = 10
keep_history_count = 5

[env]
read_env_file = ".env"

[target.FRONT_BACK]
type = "composite"
targets = "PRG1, PRG2"

[target.PRG1]
type = "java-spring-boot"
kind = "service"
module = "panda-core"
watch = ["modules/panda-core/src", "modules/shared"]
port.http.type = "http"
port.http.auto_range = "8000-8100"
port.debug.type = "tcp"
port.debug.auto = true
env.ADD_THIS_ENVVAR = "something"
env.ADD_A_REFERENCE_TO_ANOTHER_TARGETS_PORT = "{PRG2:port.http}"
env.ADD_WHATEVER_TO_ENV = "something else"

[target.PRG2]
type = "node-script"
kind = "job"
timeout_seconds = 120
port.metrics.type = "tcp"
port.metrics.fixed = 9100
"#;

        let config = load_from_str(fixture_path(), toml).expect("config parsed");

        assert_eq!(config.log.file, PathBuf::from("/tmp/rufa/rufa.log"));
        assert_eq!(
            config.env.read_env_file,
            Some(PathBuf::from("/tmp/rufa/.env"))
        );
        assert_eq!(config.log.rotation_after, Some(Duration::from_secs(86400)));
        assert_eq!(config.log.rotation_size_bytes, Some(10 * 1024 * 1024));
        assert_eq!(config.log.keep_history_count, 5);
        assert_eq!(config.watch.stability, Duration::from_secs(10));
        let debug_update = config.debug_update.as_ref().expect("debug update");
        assert_eq!(debug_update.prefix, "RUFA");
        assert_eq!(
            debug_update.vscode.as_deref(),
            Some(Path::new("/tmp/rufa/.vscode/launch.json"))
        );
        assert_eq!(
            debug_update.zed.as_deref(),
            Some(Path::new("/tmp/rufa/.zed/debug.json"))
        );

        assert_eq!(config.targets.len(), 3);

        assert!(config.targets["FRONT_BACK"].is_composite());

        let composite = match &config.targets["FRONT_BACK"] {
            Target::Composite(target) => target,
            _ => panic!("expected composite"),
        };
        assert_eq!(composite.name, "FRONT_BACK");
        assert_eq!(composite.members, vec!["PRG1", "PRG2"]);
        assert!(composite.properties.is_empty());

        let prg1 = match &config.targets["PRG1"] {
            Target::Unit(unit) => unit,
            _ => panic!("expected unit target"),
        };
        assert!(prg1.behavior.is_service());
        assert_eq!(prg1.driver_type, "java-spring-boot");
        assert_eq!(prg1.name, "PRG1");
        assert!(
            prg1.watch_paths
                .iter()
                .any(|path| path.ends_with("modules/panda-core/src"))
        );
        assert!(
            prg1.watch_paths
                .iter()
                .any(|path| path.ends_with("modules/shared"))
        );
        assert_eq!(
            prg1.refresh_watch_type,
            RefreshWatchType::PreferRuntimeSupplied
        );
        let http_port = prg1.ports.get("http").expect("http port");
        assert_eq!(http_port.protocol, PortProtocol::Http);
        assert_eq!(
            http_port.selection,
            PortSelection::AutoRange {
                start: 8000,
                end: 8100
            }
        );
        assert_eq!(http_port.selection.as_range(), (8000, 8100));
        let debug_port = prg1.ports.get("debug").expect("debug port");
        assert_eq!(debug_port.protocol, PortProtocol::Tcp);
        assert_eq!(debug_port.selection, PortSelection::Auto);
        assert_eq!(
            prg1.env.get("ADD_THIS_ENVVAR").unwrap().as_literal(),
            Some("something")
        );
        match prg1.env.get("ADD_A_REFERENCE_TO_ANOTHER_TARGETS_PORT") {
            Some(EnvValue::Reference(reference)) => {
                assert_eq!(reference.target, "PRG2");
                assert_eq!(
                    reference.resource,
                    ReferenceResource::Port {
                        name: "http".to_string()
                    }
                );
            }
            other => panic!("expected reference, got {other:?}"),
        }
        assert_eq!(
            prg1.properties
                .get("module")
                .and_then(|value| value.as_str()),
            Some("panda-core")
        );

        let prg2 = match &config.targets["PRG2"] {
            Target::Unit(unit) => unit,
            _ => panic!("expected unit target"),
        };
        match &prg2.behavior {
            TargetBehavior::Job { timeout } => {
                assert_eq!(*timeout, Some(Duration::from_secs(120)));
            }
            _ => panic!("expected job"),
        }
        assert!(prg2.behavior.is_job());
        let metrics_port = prg2.ports.get("metrics").expect("metrics port");
        assert_eq!(metrics_port.protocol, PortProtocol::Tcp);
        assert_eq!(metrics_port.selection, PortSelection::Fixed(9100));
    }

    #[test]
    fn rejects_invalid_env_reference() {
        let toml = r#"
[target.app]
type = "java"
env.BAD = "{missing}"
"#;
        let error = load_from_str(fixture_path(), toml).unwrap_err();
        match error {
            ConfigError::InvalidEnvReference { .. } => {}
            other => panic!("unexpected error {other:?}"),
        }
    }

    #[test]
    fn load_from_path_reads_file() {
        let mut temp = NamedTempFile::new().expect("temp file");
        writeln!(temp, "[log]\nfile = \"out.log\"").unwrap();
        let temp_path = temp.into_temp_path();
        let path_buf = temp_path.to_path_buf();
        let config = load_from_path(&path_buf).expect("config loads");
        let expected_file = path_buf.parent().unwrap().join("out.log");
        assert_eq!(config.log.file, expected_file);
    }

    #[test]
    fn load_from_path_missing_file_returns_read_failure() {
        let path = PathBuf::from("/nonexistent/rufa/config.toml");
        let error = load_from_path(&path).unwrap_err();
        match error {
            ConfigError::ReadFailure { .. } => {}
            other => panic!("expected ReadFailure, got {other:?}"),
        }
    }

    #[test]
    fn parses_refresh_watch_type() {
        let toml = r#"
[target.service]
type = "bun"
kind = "service"
script = "bun run start"
refresh_watch_type = "RUFA"
"#;

        let config = load_from_str(fixture_path(), toml).expect("config parsed");
        let unit = match &config.targets["service"] {
            Target::Unit(unit) => unit,
            _ => panic!("expected unit target"),
        };
        assert_eq!(unit.refresh_watch_type, RefreshWatchType::Rufa);
    }

    #[test]
    fn rejects_unknown_refresh_watch_type() {
        let toml = r#"
[target.service]
type = "bun"
kind = "service"
script = "bun run start"
refresh_watch_type = "INVALID"
"#;

        let err = load_from_str(fixture_path(), toml).unwrap_err();
        match err {
            ConfigError::UnknownRefreshWatchType { .. } => {}
            _ => panic!("expected UnknownRefreshWatchType"),
        }
    }
}

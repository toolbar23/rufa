use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use tokio::{
    sync::{Mutex, RwLock},
    task,
};

use crate::{
    config::{self, DebugUpdateConfig, PortDefinition, Target},
    state::RuntimeState,
};

const DEFAULT_HOST: &str = "127.0.0.1";

#[derive(Debug)]
pub struct DebugUpdateContext {
    config_path: PathBuf,
    settings: DebugUpdateConfig,
    state: Arc<RwLock<RuntimeState>>,
    lock: Arc<Mutex<()>>,
}

impl DebugUpdateContext {
    pub fn new(
        config_path: PathBuf,
        settings: DebugUpdateConfig,
        state: Arc<RwLock<RuntimeState>>,
    ) -> Self {
        Self {
            config_path,
            settings,
            state,
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub async fn refresh(&self) -> Result<()> {
        let _guard = self.lock.lock().await;
        let config = config::load_from_path(&self.config_path)
            .context("loading configuration for debug update")?;

        let entries = {
            let state_guard = self.state.read().await;
            collect_debug_entries(&config, &state_guard)
        };

        if let Some(path) = self.settings.vscode.as_deref() {
            update_vscode_launch(path, &self.settings.prefix, &entries).await?;
        }

        if let Some(path) = self.settings.zed.as_deref() {
            update_zed_launch(path, &self.settings.prefix, &entries).await?;
        }

        Ok(())
    }
}

struct DebugEntry {
    target: String,
    port_name: String,
    port: u16,
    vscode: Option<JsonValue>,
    zed: Option<JsonValue>,
}

struct DebugEntrySpec {
    port_name: &'static str,
    vscode: Option<JsonValue>,
    zed: Option<JsonValue>,
}

struct DebugTemplates {
    vscode: Option<JsonValue>,
    zed: Option<JsonValue>,
}

struct PlaceholderContext<'a> {
    prefix: &'a str,
    target: &'a str,
    port_name: &'a str,
    port: u16,
    host: &'a str,
}

fn collect_debug_entries(config: &config::Config, state: &RuntimeState) -> Vec<DebugEntry> {
    let mut entries = Vec::new();

    for (target_name, target_state) in &state.targets {
        if target_state.pid.is_none() {
            continue;
        }

        let Some(Target::Unit(unit)) = config.targets.get(target_name) else {
            continue;
        };

        if let Some(specs) = builtin_debug_specs(&unit.driver_type) {
            for spec in specs {
                if !unit.ports.contains_key(spec.port_name) {
                    continue;
                }

                let Some(port) = target_state.ports.get(spec.port_name) else {
                    continue;
                };

                entries.push(DebugEntry {
                    target: target_name.clone(),
                    port_name: spec.port_name.to_string(),
                    port: *port,
                    vscode: spec.vscode.clone(),
                    zed: spec.zed.clone(),
                });
            }
            continue;
        }

        if unit.driver_type == "bash" {
            for (port_name, definition) in &unit.ports {
                let Some(port) = target_state.ports.get(port_name) else {
                    continue;
                };

                if let Some(templates) = extract_debug_templates(definition) {
                    entries.push(DebugEntry {
                        target: target_name.clone(),
                        port_name: port_name.clone(),
                        port: *port,
                        vscode: templates.vscode,
                        zed: templates.zed,
                    });
                }
            }
        }
    }

    entries
}

fn builtin_debug_specs(driver_type: &str) -> Option<Vec<DebugEntrySpec>> {
    match driver_type {
        "java-spring-boot" => Some(vec![DebugEntrySpec {
            port_name: "debug",
            vscode: Some(json!({
                "type": "java",
                "request": "attach",
                "hostName": "{{HOST}}",
                "port": "{{PORT}}",
            })),
            zed: Some(json!({
                "adapter": "Java",
                "request": "attach",
                "tcp_connection": {
                    "host": "{{HOST}}",
                    "port": "{{PORT}}"
                }
            })),
        }]),
        "bun" => Some(vec![DebugEntrySpec {
            port_name: "debug",
            vscode: Some(json!({
                "type": "pwa-node",
                "request": "attach",
                "address": "{{HOST}}",
                "port": "{{PORT}}",
                "localRoot": "${workspaceFolder}",
                "remoteRoot": "${workspaceFolder}",
            })),
            zed: Some(json!({
                "adapter": "JavaScript",
                "request": "attach",
                "tcp_connection": {
                    "host": "{{HOST}}",
                    "port": "{{PORT}}"
                }
            })),
        }]),
        _ => None,
    }
}

fn extract_debug_templates(definition: &PortDefinition) -> Option<DebugTemplates> {
    let Some(value) = definition.properties.get("debugger") else {
        return None;
    };

    let toml::Value::Table(table) = value else {
        return None;
    };

    let vscode = table.get("vscode").map(|value| toml_to_json(value));
    let zed = table.get("zed").map(|value| toml_to_json(value));

    if vscode.is_none() && zed.is_none() {
        return None;
    }

    Some(DebugTemplates { vscode, zed })
}

fn toml_to_json(value: &toml::Value) -> JsonValue {
    match value {
        toml::Value::String(s) => JsonValue::String(s.clone()),
        toml::Value::Integer(i) => json!(*i),
        toml::Value::Float(f) => json!(*f),
        toml::Value::Boolean(b) => JsonValue::Bool(*b),
        toml::Value::Datetime(dt) => JsonValue::String(dt.to_string()),
        toml::Value::Array(array) => JsonValue::Array(array.iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => {
            let mut map = JsonMap::new();
            for (key, value) in table {
                map.insert(key.clone(), toml_to_json(value));
            }
            JsonValue::Object(map)
        }
    }
}

async fn update_vscode_launch(path: &Path, prefix: &str, entries: &[DebugEntry]) -> Result<()> {
    let path = path.to_path_buf();
    let prefix = prefix.to_string();
    let rendered: Vec<JsonValue> = entries
        .iter()
        .filter_map(|entry| render_vscode_entry(entry, &prefix))
        .collect();

    task::spawn_blocking(move || update_vscode_launch_sync(&path, &prefix, rendered))
        .await
        .context("failed to join VS Code launch update task")??;

    Ok(())
}

async fn update_zed_launch(path: &Path, prefix: &str, entries: &[DebugEntry]) -> Result<()> {
    let path = path.to_path_buf();
    let prefix = prefix.to_string();
    let rendered: Vec<JsonValue> = entries
        .iter()
        .filter_map(|entry| render_zed_entry(entry, &prefix))
        .collect();

    task::spawn_blocking(move || update_zed_launch_sync(&path, &prefix, rendered))
        .await
        .context("failed to join Zed debug update task")??;

    Ok(())
}

fn update_vscode_launch_sync(path: &Path, prefix: &str, new_entries: Vec<JsonValue>) -> Result<()> {
    let existing = read_json_file(path)?;

    let mut root = match existing {
        Some(JsonValue::Object(map)) => map,
        Some(_) => JsonMap::new(),
        None => {
            let mut map = JsonMap::new();
            map.insert(
                "version".to_string(),
                JsonValue::String("0.2.0".to_string()),
            );
            map
        }
    };

    let configurations = root
        .entry("configurations".to_string())
        .or_insert_with(|| JsonValue::Array(Vec::new()));

    let mut preserved = Vec::new();
    if let JsonValue::Array(existing_configs) = configurations {
        for config in existing_configs {
            let keep = config
                .as_object()
                .and_then(|map| map.get("name"))
                .and_then(|value| value.as_str())
                .map(|name| !name.starts_with(prefix))
                .unwrap_or(true);
            if keep {
                preserved.push(config.clone());
            }
        }
    }

    preserved.extend(new_entries);
    root.insert("configurations".to_string(), JsonValue::Array(preserved));

    let updated = JsonValue::Object(root);
    write_json_if_changed(path, updated)
}

fn update_zed_launch_sync(path: &Path, prefix: &str, new_entries: Vec<JsonValue>) -> Result<()> {
    let existing = read_json_file(path)?;

    let mut entries = match existing {
        Some(JsonValue::Array(array)) => array,
        Some(_) => Vec::new(),
        None => Vec::new(),
    };

    entries.retain(|entry| {
        entry
            .as_object()
            .and_then(|map| map.get("label"))
            .and_then(|value| value.as_str())
            .map(|label| !label.starts_with(prefix))
            .unwrap_or(true)
    });

    entries.extend(new_entries);

    write_json_if_changed(path, JsonValue::Array(entries))
}

fn read_json_file(path: &Path) -> Result<Option<JsonValue>> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            if contents.trim().is_empty() {
                Ok(None)
            } else {
                let value = serde_json::from_str(&contents)
                    .with_context(|| format!("parsing JSON from {}", path.display()))?;
                Ok(Some(value))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_json_if_changed(path: &Path, value: JsonValue) -> Result<()> {
    let new_contents = serde_json::to_string_pretty(&value)?;
    let write_needed = match fs::read_to_string(path) {
        Ok(existing) => existing.trim_end() != new_contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };

    if write_needed {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
        fs::write(path, format!("{}\n", new_contents))
            .with_context(|| format!("writing {}", path.display()))?;
    }

    Ok(())
}

fn render_vscode_entry(entry: &DebugEntry, prefix: &str) -> Option<JsonValue> {
    let template = entry.vscode.as_ref()?;
    let mut value = template.clone();

    let context = PlaceholderContext {
        prefix,
        target: &entry.target,
        port_name: &entry.port_name,
        port: entry.port,
        host: DEFAULT_HOST,
    };

    apply_placeholders(&mut value, &context);

    let mut map = match value {
        JsonValue::Object(map) => map,
        _ => JsonMap::new(),
    };

    let label = format_label(prefix, &entry.target, &entry.port_name);
    map.insert("name".to_string(), JsonValue::String(label));
    map.insert("port".to_string(), JsonValue::from(entry.port));

    map.entry("address".to_string())
        .or_insert_with(|| JsonValue::String(DEFAULT_HOST.to_string()));
    map.entry("host".to_string())
        .or_insert_with(|| JsonValue::String(DEFAULT_HOST.to_string()));

    Some(JsonValue::Object(map))
}

fn render_zed_entry(entry: &DebugEntry, prefix: &str) -> Option<JsonValue> {
    let template = entry.zed.as_ref()?;
    let mut value = template.clone();

    let context = PlaceholderContext {
        prefix,
        target: &entry.target,
        port_name: &entry.port_name,
        port: entry.port,
        host: DEFAULT_HOST,
    };

    apply_placeholders(&mut value, &context);

    let mut map = match value {
        JsonValue::Object(map) => map,
        _ => JsonMap::new(),
    };

    let label = format_label(prefix, &entry.target, &entry.port_name);
    map.insert("label".to_string(), JsonValue::String(label));

    let tcp_connection = map
        .entry("tcp_connection".to_string())
        .or_insert_with(|| JsonValue::Object(JsonMap::new()));

    match tcp_connection {
        JsonValue::Object(tcp) => {
            tcp.entry("host".to_string())
                .or_insert_with(|| JsonValue::String(DEFAULT_HOST.to_string()));
            tcp.insert("port".to_string(), JsonValue::from(entry.port));
        }
        _ => {
            let mut tcp = JsonMap::new();
            tcp.insert(
                "host".to_string(),
                JsonValue::String(DEFAULT_HOST.to_string()),
            );
            tcp.insert("port".to_string(), JsonValue::from(entry.port));
            map.insert("tcp_connection".to_string(), JsonValue::Object(tcp));
        }
    }

    Some(JsonValue::Object(map))
}

fn apply_placeholders(value: &mut JsonValue, context: &PlaceholderContext<'_>) {
    match value {
        JsonValue::String(string) => {
            let replacements = [
                ("{{TARGET}}", context.target),
                ("{{PORT}}", &context.port.to_string()),
                ("{{PORT_NAME}}", context.port_name),
                ("{{PREFIX}}", context.prefix),
                ("{{HOST}}", context.host),
            ];

            for (token, replacement) in replacements {
                if string.contains(token) {
                    *string = string.replace(token, replacement);
                }
            }
        }
        JsonValue::Array(array) => {
            for item in array {
                apply_placeholders(item, context);
            }
        }
        JsonValue::Object(map) => {
            for value in map.values_mut() {
                apply_placeholders(value, context);
            }
        }
        _ => {}
    }
}

fn format_label(prefix: &str, target: &str, port_name: &str) -> String {
    format!("{} {} ({})", prefix, target, port_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config,
        state::{RuntimeState, TargetState},
    };
    use serde_json::json;
    use std::path::Path;
    use tempfile::tempdir;

    #[test]
    fn render_vscode_entry_sets_defaults() {
        let entry = DebugEntry {
            target: "PRG1".to_string(),
            port_name: "debug".to_string(),
            port: 5005,
            vscode: Some(json!({
                "type": "pwa-node",
                "request": "attach",
                "address": "{{HOST}}",
            })),
            zed: None,
        };

        let rendered = render_vscode_entry(&entry, "RUFA").expect("rendered config");
        let map = rendered.as_object().expect("object");
        assert_eq!(
            map.get("name").and_then(JsonValue::as_str),
            Some("RUFA PRG1 (debug)")
        );
        assert_eq!(map.get("port").and_then(JsonValue::as_u64), Some(5005));
        assert_eq!(
            map.get("address").and_then(JsonValue::as_str),
            Some(DEFAULT_HOST)
        );
        assert_eq!(
            map.get("host").and_then(JsonValue::as_str),
            Some(DEFAULT_HOST)
        );
        assert_eq!(
            map.get("type").and_then(JsonValue::as_str),
            Some("pwa-node")
        );
        assert_eq!(
            map.get("request").and_then(JsonValue::as_str),
            Some("attach")
        );
    }

    #[test]
    fn update_vscode_launch_sync_rewrites_prefixed_entries() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("launch.json");
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "version": "0.2.0",
                "configurations": [
                    {"name": "RUFA Old (debug)", "type": "pwa-node", "request": "attach", "port": 4000},
                    {"name": "Manual", "type": "lldb", "request": "launch"}
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let new_entries = vec![json!({
            "name": "RUFA PRG1 (debug)",
            "type": "pwa-node",
            "request": "attach",
            "port": 5005,
            "address": DEFAULT_HOST,
        })];

        update_vscode_launch_sync(&path, "RUFA", new_entries).expect("update");

        let updated: JsonValue = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let configs = updated
            .get("configurations")
            .and_then(JsonValue::as_array)
            .expect("configurations array");
        assert_eq!(configs.len(), 2);
        assert!(
            configs
                .iter()
                .any(|cfg| cfg.get("name").and_then(JsonValue::as_str) == Some("Manual"))
        );
        assert!(
            configs.iter().any(
                |cfg| cfg.get("name").and_then(JsonValue::as_str) == Some("RUFA PRG1 (debug)")
            )
        );
    }

    #[test]
    fn update_zed_launch_sync_rewrites_prefixed_entries() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("debug.json");
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!([
                {"label": "RUFA Old (debug)", "adapter": "Debugpy"},
                {"label": "Keep", "adapter": "CodeLLDB"}
            ]))
            .unwrap(),
        )
        .unwrap();

        let new_entries = vec![json!({
            "label": "RUFA PRG1 (debug)",
            "adapter": "CodeLLDB",
            "tcp_connection": {"host": DEFAULT_HOST, "port": 6006},
        })];

        update_zed_launch_sync(&path, "RUFA", new_entries).expect("update");

        let updated: JsonValue = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let configs = updated.as_array().expect("array");
        assert_eq!(configs.len(), 2);
        assert!(
            configs
                .iter()
                .any(|cfg| cfg.get("label").and_then(JsonValue::as_str) == Some("Keep"))
        );
        assert!(
            configs.iter().any(
                |cfg| cfg.get("label").and_then(JsonValue::as_str) == Some("RUFA PRG1 (debug)")
            )
        );
    }

    #[test]
    fn collect_debug_entries_uses_builtin_templates() {
        let toml = r#"
[target.APP]
type = "java-spring-boot"
kind = "service"
port.debug.type = "tcp"
port.debug.auto = true
"#;

        let config = config::load_from_str(Path::new("/tmp/rufa/rufa.toml"), toml).expect("config");

        let mut state = RuntimeState::default();
        let mut ports = std::collections::HashMap::new();
        ports.insert("debug".to_string(), 5005);
        state.targets.insert(
            "APP".to_string(),
            TargetState {
                generation: 1,
                pid: Some(1234),
                ports,
                ..Default::default()
            },
        );

        let entries = collect_debug_entries(&config, &state);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.target, "APP");
        assert_eq!(entry.port_name, "debug");
        assert_eq!(entry.port, 5005);
        assert!(entry.vscode.is_some());
        assert!(entry.zed.is_some());
    }
}

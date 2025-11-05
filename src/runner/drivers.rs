use anyhow::{Result, anyhow, bail};
use once_cell::sync::Lazy;
use std::{collections::HashMap, sync::Arc};

use crate::config::model::UnitTarget;

pub(crate) trait TargetDriver: Sync + Send {
    fn resolve_command(&self, unit: &UnitTarget, runtime_watch: bool) -> Result<String>;
}

pub(crate) fn resolve_command(unit: &UnitTarget, runtime_watch: bool) -> Result<String> {
    let key = unit.driver_type.to_ascii_lowercase();
    if let Some(driver) = DRIVER_REGISTRY.get(key.as_str()) {
        driver.resolve_command(unit, runtime_watch)
    } else {
        TargetDriver::resolve_command(&GENERIC_DRIVER, unit, runtime_watch)
    }
}

static DRIVER_REGISTRY: Lazy<HashMap<&'static str, Arc<dyn TargetDriver>>> = Lazy::new(|| {
    let mut map: HashMap<&'static str, Arc<dyn TargetDriver>> = HashMap::new();
    map.insert("java-spring-boot", Arc::new(JavaSpringBootDriver));
    map.insert("bun", Arc::new(BunDriver));
    map.insert("bash", Arc::new(BashDriver));
    map.insert("rust", Arc::new(RustDriver));
    map.insert("docker-compose", Arc::new(DockerComposeDriver));
    map
});

static GENERIC_DRIVER: GenericDriver = GenericDriver;

struct JavaSpringBootDriver;

impl TargetDriver for JavaSpringBootDriver {
    fn resolve_command(&self, unit: &UnitTarget, runtime_watch: bool) -> Result<String> {
        if let Some(command) = unit
            .properties
            .get("command")
            .and_then(|value| value.as_str())
        {
            return Ok(adjust_runtime_watch_flag(
                command,
                runtime_watch,
                " --spring-boot-devtools",
            )
            .to_string());
        }

        if let Some(module) = unit
            .properties
            .get("module")
            .and_then(|value| value.as_str())
        {
            // Default to Maven Spring Boot plugin.
            let mut jvm_args =
                "-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:${PORT_DEBUG}"
                    .to_string();
            if runtime_watch {
                jvm_args.push(' ');
                jvm_args.push_str("-Dspring.devtools.restart.enabled=true");
            }
            let command = format!(
                "mvn -pl {module} spring-boot:run -Dspring-boot.run.jvmArguments=\"{args}\"",
                module = module,
                args = jvm_args
            );
            return Ok(command);
        }

        if let Some(jar) = unit.properties.get("jar").and_then(|value| value.as_str()) {
            return Ok(format!("java -jar {}", jar));
        }

        bail!(
            "target {} (java-spring-boot) must define `command`, `module`, or `jar` property",
            unit.name
        );
    }
}

struct BunDriver;

impl TargetDriver for BunDriver {
    fn resolve_command(&self, unit: &UnitTarget, runtime_watch: bool) -> Result<String> {
        let command = resolve_script_property(unit, "script")?;
        if runtime_watch && !command.contains("--watch") {
            Ok(format!("{command} --watch"))
        } else {
            Ok(command)
        }
    }
}

struct BashDriver;

impl TargetDriver for BashDriver {
    fn resolve_command(&self, unit: &UnitTarget, _runtime_watch: bool) -> Result<String> {
        resolve_script_property(unit, "command")
    }
}

struct RustDriver;

impl TargetDriver for RustDriver {
    fn resolve_command(&self, unit: &UnitTarget, _runtime_watch: bool) -> Result<String> {
        if let Some(command) = unit
            .properties
            .get("command")
            .and_then(|value| value.as_str())
        {
            return Ok(command.to_string());
        }

        let mut base = String::from("cargo run");
        if let Some(bin) = unit.properties.get("bin").and_then(|value| value.as_str()) {
            base.push_str(" --bin ");
            base.push_str(bin);
        }
        if let Some(package) = unit
            .properties
            .get("package")
            .and_then(|value| value.as_str())
        {
            base.push_str(" -p ");
            base.push_str(package);
        }
        Ok(base)
    }
}

struct GenericDriver;

impl TargetDriver for GenericDriver {
    fn resolve_command(&self, unit: &UnitTarget, _runtime_watch: bool) -> Result<String> {
        if let Some(command) = unit
            .properties
            .get("command")
            .and_then(|value| value.as_str())
        {
            return Ok(command.to_string());
        }

        if let Some(script) = unit
            .properties
            .get("script")
            .and_then(|value| value.as_str())
        {
            return Ok(script.to_string());
        }

        bail!(
            "target {} is missing a `command` or `script` property",
            unit.name
        );
    }
}

struct DockerComposeDriver;

impl TargetDriver for DockerComposeDriver {
    fn resolve_command(&self, unit: &UnitTarget, _runtime_watch: bool) -> Result<String> {
        let compose_file = unit
            .properties
            .get("compose_file")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                anyhow!(
                    "target {} (docker-compose) requires `compose_file` property",
                    unit.name
                )
            })?;

        Ok(format!(
            "docker-compose --file={} up --abort-on-container-exit --attach-dependencies -y",
            compose_file
        ))
    }
}

fn resolve_script_property(unit: &UnitTarget, key: &str) -> Result<String> {
    unit.properties
        .get(key)
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .ok_or_else(|| {
            anyhow!(
                "target {} requires `{}` property in configuration",
                unit.name,
                key
            )
        })
}

fn adjust_runtime_watch_flag(command: &str, runtime_watch: bool, flag: &str) -> String {
    if runtime_watch && !command.contains(flag.trim()) {
        format!("{command}{flag}")
    } else {
        command.to_string()
    }
}

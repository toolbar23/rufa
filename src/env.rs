use anyhow::{Result, anyhow};
use std::{collections::HashMap, env as std_env, fs, io, path::Path};

pub const OVERRIDE_ENV_FILE_VAR: &str = "RUFA_ENV_FILE";

/// Load overrides from a .env-style file, tolerating comments and blank lines.
pub fn load_env_overrides(path: &Path) -> Result<HashMap<String, String>> {
    parse_lenient_dotenv(path).map_err(|error| {
        anyhow!(
            "failed to parse environment overrides from {:?}: {error}",
            path
        )
    })
}

/// Apply the provided environment overrides, returning the previous values so callers can restore.
#[allow(dead_code)]
pub fn apply_env(overrides: &HashMap<String, String>) -> Result<Vec<(String, Option<String>)>> {
    let mut previous = Vec::with_capacity(overrides.len());
    for (key, value) in overrides {
        let prior = std_env::var(key).ok();
        previous.push((key.clone(), prior));
        unsafe {
            std_env::set_var(key, value);
        }
    }
    Ok(previous)
}

/// Restore the environment values that were replaced by `apply_env`.
#[allow(dead_code)]
pub fn restore_env(previous: Vec<(String, Option<String>)>) {
    for (key, value) in previous {
        match value {
            Some(original) => unsafe { std_env::set_var(key, original) },
            None => unsafe { std_env::remove_var(key) },
        }
    }
}

fn parse_lenient_dotenv(path: &Path) -> io::Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    let contents = fs::read_to_string(path)?;

    for (idx, raw) in contents.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut parts = line.splitn(2, '=');
        let Some(key) = parts.next() else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }

        if key.starts_with('.') {
            continue;
        }

        let value = parts.next().unwrap_or_default().trim();
        if !key.chars().all(is_env_key_char) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid environment variable name `{}` at line {}",
                    key,
                    idx + 1
                ),
            ));
        }

        map.insert(key.to_string(), strip_quotes(value));
    }

    Ok(map)
}

fn is_env_key_char(ch: char) -> bool {
    matches!(ch, '_' | '-' | '.') || ch.is_ascii_alphanumeric()
}

fn strip_quotes(value: &str) -> String {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn load_env_overrides_accepts_dot_names() -> Result<()> {
        let file = NamedTempFile::new()?;
        fs::write(
            file.path(),
            "FOO=bar\n.BAZ=ignored\nQUX = quux\nwith-dash=value\nWITH.DOT=found\n",
        )?;
        let overrides = load_env_overrides(file.path())?;
        assert_eq!(overrides.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(overrides.get("QUX"), Some(&"quux".to_string()));
        assert!(!overrides.contains_key(".BAZ"));
        assert_eq!(overrides.get("with-dash"), Some(&"value".to_string()));
        assert_eq!(overrides.get("WITH.DOT"), Some(&"found".to_string()));
        Ok(())
    }

    #[test]
    fn apply_and_restore_env_round_trip() -> Result<()> {
        unsafe {
            std_env::remove_var("TEST_ENV_KEY");
        }
        let mut overrides = HashMap::new();
        overrides.insert("TEST_ENV_KEY".to_string(), "value".to_string());

        let previous = apply_env(&overrides)?;
        assert_eq!(std_env::var("TEST_ENV_KEY")?, "value");

        restore_env(previous);
        assert!(std_env::var("TEST_ENV_KEY").is_err());
        Ok(())
    }
}

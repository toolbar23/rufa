use std::{fs, path::PathBuf};

use crate::ipc::runtime_dir as ipc_runtime_dir;

pub fn runtime_dir() -> PathBuf {
    ipc_runtime_dir()
}

pub fn logs_dir() -> PathBuf {
    runtime_dir().join("logs")
}

pub fn ensure_logs_dir() -> std::io::Result<PathBuf> {
    let dir = logs_dir();
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn targets_dir() -> PathBuf {
    runtime_dir().join("targets")
}

pub fn ensure_targets_dir() -> std::io::Result<PathBuf> {
    let dir = targets_dir();
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn sanitize_target_name(name: &str) -> String {
    let mut sanitized = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }
    sanitized
}

pub fn log_file_name(target: &str, generation: u64) -> String {
    format!("{}.{}.log", sanitize_target_name(target), generation)
}

pub fn log_file_path(target: &str, generation: u64) -> PathBuf {
    logs_dir().join(log_file_name(target, generation))
}

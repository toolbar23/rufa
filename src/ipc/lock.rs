use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Information persisted to `.rufa.lock` to coordinate with the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    pub pid: u32,
    pub socket_path: String,
    #[serde(default)]
    pub refresh_target_on_change: bool,
}

impl LockInfo {
    pub fn socket_path(&self) -> &Path {
        Path::new(&self.socket_path)
    }
}

pub fn lock_file_path() -> PathBuf {
    PathBuf::from(".rufa.lock")
}

pub fn runtime_dir() -> PathBuf {
    PathBuf::from(".rufa")
}

pub fn ensure_runtime_dir() -> io::Result<PathBuf> {
    let dir = runtime_dir();
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn allocate_socket_path() -> io::Result<PathBuf> {
    let dir = ensure_runtime_dir()?;
    Ok(dir.join(format!("rufa-{}.sock", Uuid::new_v4())))
}

pub fn read_lock() -> io::Result<Option<LockInfo>> {
    let path = lock_file_path();
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path)?;
    let info: LockInfo = serde_json::from_str(&contents)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    Ok(Some(info))
}

pub fn write_lock(info: &LockInfo) -> io::Result<()> {
    let path = lock_file_path();
    let tmp_path = path.with_extension("tmp");
    let contents =
        serde_json::to_vec(info).map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
    fs::write(&tmp_path, contents)?;
    fs::rename(&tmp_path, &path)?;
    Ok(())
}

pub fn cleanup_lock(info: &LockInfo) -> io::Result<()> {
    let _ = remove_socket(info.socket_path());
    let _ = remove_lock_file();
    Ok(())
}

pub fn remove_lock_file() -> io::Result<()> {
    let path = lock_file_path();
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn remove_socket(path: &Path) -> io::Result<()> {
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn update_refresh_target_on_change(value: bool) -> io::Result<()> {
    if let Some(mut info) = read_lock()? {
        info.refresh_target_on_change = value;
        write_lock(&info)?;
    }
    Ok(())
}

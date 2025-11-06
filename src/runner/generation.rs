use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug)]
pub struct TargetGenerationStore {
    base_dir: PathBuf,
    generations: HashMap<String, u64>,
}

impl TargetGenerationStore {
    pub fn new() -> io::Result<Self> {
        let base_dir = paths::ensure_targets_dir()?;
        let mut generations = HashMap::new();
        for entry in fs::read_dir(&base_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            match read_record(&path) {
                Ok(record) => {
                    generations.insert(record.target, record.generation);
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        path = %path.display(),
                        "failed to load target generation record"
                    );
                }
            }
        }
        Ok(Self {
            base_dir,
            generations,
        })
    }

    pub fn next(&mut self, target: &str) -> io::Result<u64> {
        let key = target.to_string();
        let new_value = self
            .generations
            .get(&key)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        self.generations.insert(key, new_value);
        self.persist(target, new_value)?;
        Ok(new_value)
    }

    pub fn current(&self, target: &str) -> Option<u64> {
        self.generations.get(target).copied()
    }

    pub fn all(&self) -> Vec<(String, u64)> {
        let mut items = self
            .generations
            .iter()
            .map(|(target, generation)| (target.clone(), *generation))
            .collect::<Vec<_>>();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        items
    }

    fn persist(&self, target: &str, generation: u64) -> io::Result<()> {
        let path = record_path(&self.base_dir, target);
        let tmp_path = tmp_path(&path);
        let record = TargetGenerationRecord {
            target: target.to_string(),
            generation,
        };
        let contents =
            serde_json::to_vec(&record).map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
        fs::write(&tmp_path, contents)?;
        fs::rename(&tmp_path, &path)?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TargetGenerationRecord {
    target: String,
    generation: u64,
}

fn read_record(path: &Path) -> io::Result<TargetGenerationRecord> {
    let contents = fs::read_to_string(path)?;
    let record = serde_json::from_str(&contents)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    Ok(record)
}

fn record_path(base_dir: &Path, target: &str) -> PathBuf {
    base_dir.join(format!("{}.json", paths::sanitize_target_name(target)))
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("tmp")
        .to_string();
    name.push_str(".tmp");
    path.with_file_name(name)
}

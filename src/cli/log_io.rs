use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, VecDeque},
    fs,
    io::{self, IsTerminal},
    path::PathBuf,
};

use anyhow::Result;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use tokio::time::Duration as TokioDuration;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::paths;

#[derive(Clone, Debug)]
pub struct LogSource {
    pub target: String,
    pub generation: u64,
}

impl LogSource {
    pub fn new(target: impl Into<String>, generation: u64) -> Self {
        Self {
            target: target.into(),
            generation,
        }
    }

    pub fn path(&self) -> PathBuf {
        paths::log_file_path(&self.target, self.generation)
    }
}

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub timestamp: DateTime<Utc>,
    pub target: String,
    pub generation: u64,
    pub stream: String,
    pub message: String,
}

#[derive(Clone, Debug)]
struct HeapItem {
    timestamp: DateTime<Utc>,
    source_index: usize,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.timestamp == other.timestamp && self.source_index == other.source_index
    }
}

impl Eq for HeapItem {}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.timestamp
            .cmp(&other.timestamp)
            .then(self.source_index.cmp(&other.source_index))
    }
}

pub fn read_entries(source: &LogSource) -> io::Result<Vec<LogEntry>> {
    let path = source.path();
    let contents = match fs::read_to_string(&path) {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };

    let mut entries = Vec::new();
    for line in contents.lines() {
        if let Some(entry) = parse_log_line(source, line) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

pub fn merge_entries(entry_sets: Vec<Vec<LogEntry>>) -> Vec<LogEntry> {
    if entry_sets.is_empty() {
        return Vec::new();
    }
    let mut positions = vec![0usize; entry_sets.len()];
    let mut heap: BinaryHeap<Reverse<HeapItem>> = BinaryHeap::new();

    for (index, entries) in entry_sets.iter().enumerate() {
        if let Some(entry) = entries.first() {
            heap.push(Reverse(HeapItem {
                timestamp: entry.timestamp,
                source_index: index,
            }));
        }
    }

    let mut merged = Vec::new();
    while let Some(Reverse(item)) = heap.pop() {
        let idx = item.source_index;
        let pos = positions[idx];
        if let Some(entry) = entry_sets[idx].get(pos) {
            merged.push(entry.clone());
            positions[idx] += 1;
            if let Some(next) = entry_sets[idx].get(positions[idx]) {
                heap.push(Reverse(HeapItem {
                    timestamp: next.timestamp,
                    source_index: idx,
                }));
            }
        }
    }
    merged
}

pub fn take_last(entries: Vec<LogEntry>, count: usize) -> Vec<LogEntry> {
    if count == 0 || entries.is_empty() {
        return Vec::new();
    }
    if entries.len() <= count {
        return entries;
    }
    entries
        .into_iter()
        .rev()
        .take(count)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

pub fn format_entry(entry: &LogEntry) -> String {
    format!(
        "{} | {:<18} | {:>5} | {:<7} | {}",
        entry.timestamp.to_rfc3339_opts(SecondsFormat::Millis, true),
        entry.target,
        entry.generation,
        entry.stream,
        entry.message
    )
}

pub fn tail_logs_for_target(
    target: &str,
    generation: u64,
    limit: usize,
    max_width: Option<usize>,
) -> io::Result<Vec<String>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let source = LogSource::new(target.to_string(), generation);
    let entries = read_entries(&source)?;
    let selected = take_last(entries, limit);
    let mut lines = Vec::new();
    for entry in selected {
        let formatted = format_entry(&entry);
        lines.push(truncate_line(&formatted, max_width).into_owned());
    }
    Ok(lines)
}

pub fn truncate_line<'a>(line: &'a str, max_width: Option<usize>) -> std::borrow::Cow<'a, str> {
    match max_width {
        None => std::borrow::Cow::Borrowed(line),
        Some(0) => std::borrow::Cow::Owned(String::new()),
        Some(limit) => {
            if UnicodeWidthStr::width(line) <= limit {
                std::borrow::Cow::Borrowed(line)
            } else {
                let mut acc = String::with_capacity(line.len());
                let mut width = 0;
                for ch in line.chars() {
                    let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                    if width + ch_width > limit {
                        break;
                    }
                    acc.push(ch);
                    width += ch_width;
                }
                std::borrow::Cow::Owned(acc)
            }
        }
    }
}

pub fn colorize_line(line: &str) -> String {
    if !std::io::stdout().is_terminal() {
        return line.to_string();
    }
    if line.contains("| STDERR |") || line.to_ascii_lowercase().contains("error") {
        format!("\u{001b}[31m{}\u{001b}[0m", line)
    } else if line.contains("| STDOUT |") {
        format!("\u{001b}[32m{}\u{001b}[0m", line)
    } else {
        line.to_string()
    }
}

pub fn list_generations_from_fs(target: &str) -> io::Result<Vec<u64>> {
    let dir = paths::ensure_logs_dir()?;
    let sanitized = paths::sanitize_target_name(target);
    let prefix = format!("{sanitized}.");
    let mut generations = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let file_name = entry.file_name();
            let name = match file_name.to_str() {
                Some(value) => value,
                None => continue,
            };
            if !name.starts_with(&prefix) || !name.ends_with(".log") {
                continue;
            }
            let suffix = &name[prefix.len()..name.len() - 4];
            if let Ok(value) = suffix.parse::<u64>() {
                generations.push(value);
            }
        }
    }
    generations.sort_unstable();
    generations.dedup();
    Ok(generations)
}

pub fn build_sources(
    targets: &[String],
    explicit_generation: Option<u64>,
    include_all: bool,
    generation_map: &HashMap<String, u64>,
) -> io::Result<Vec<LogSource>> {
    let mut unique_targets = if targets.is_empty() {
        let mut names: Vec<String> = generation_map.keys().cloned().collect();
        names.sort();
        names
    } else {
        let mut names = targets.to_vec();
        names.sort();
        names.dedup();
        names
    };

    if unique_targets.is_empty() {
        unique_targets = generation_map.keys().cloned().collect();
    }

    let mut sources = Vec::new();
    for target in unique_targets {
        if include_all {
            let mut generations = list_generations_from_fs(&target)?;
            if generations.is_empty() {
                if let Some(generation) = generation_map.get(&target) {
                    if *generation > 0 {
                        generations.push(*generation);
                    }
                }
            }
            for generation in generations {
                sources.push(LogSource::new(target.clone(), generation));
            }
        } else if let Some(generation) = explicit_generation {
            sources.push(LogSource::new(target.clone(), generation));
        } else if let Some(generation) = generation_map.get(&target) {
            if *generation > 0 {
                sources.push(LogSource::new(target.clone(), *generation));
            }
        } else {
            let generations = list_generations_from_fs(&target)?;
            if let Some(latest) = generations.into_iter().max() {
                sources.push(LogSource::new(target.clone(), latest));
            }
        }
    }

    Ok(sources)
}

pub async fn follow_sources(sources: Vec<LogSource>) -> Result<()> {
    if sources.is_empty() {
        return Ok(());
    }

    let mut states = Vec::new();
    for source in sources {
        let offset = match tokio::fs::metadata(source.path()).await {
            Ok(meta) => meta.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error.into()),
        };
        states.push(LogFollowerState {
            source,
            offset,
            buffer: VecDeque::new(),
        });
    }

    let mut heap: BinaryHeap<Reverse<HeapItem>> = BinaryHeap::new();
    let mut present = vec![false; states.len()];

    loop {
        for (idx, state) in states.iter_mut().enumerate() {
            state.refresh().await?;
            if !present[idx] {
                if let Some(entry) = state.buffer.front() {
                    heap.push(Reverse(HeapItem {
                        timestamp: entry.timestamp,
                        source_index: idx,
                    }));
                    present[idx] = true;
                }
            }
        }

        while let Some(Reverse(item)) = heap.pop() {
            let state = &mut states[item.source_index];
            present[item.source_index] = false;
            if let Some(entry) = state.buffer.pop_front() {
                let formatted = format_entry(&entry);
                println!("{}", colorize_line(&formatted));
                if let Some(next) = state.buffer.front() {
                    heap.push(Reverse(HeapItem {
                        timestamp: next.timestamp,
                        source_index: item.source_index,
                    }));
                    present[item.source_index] = true;
                }
            }
        }

        tokio::time::sleep(TokioDuration::from_millis(500)).await;
    }
}

pub fn read_generations_from_disk() -> io::Result<HashMap<String, u64>> {
    let dir = paths::ensure_targets_dir()?;
    let mut map = HashMap::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let contents = fs::read_to_string(&path)?;
            if let Ok(record) = serde_json::from_str::<TargetGenerationRecord>(&contents) {
                map.insert(record.target, record.generation);
            }
        }
    }
    Ok(map)
}

#[derive(Deserialize)]
struct TargetGenerationRecord {
    target: String,
    generation: u64,
}

struct LogFollowerState {
    source: LogSource,
    offset: u64,
    buffer: VecDeque<LogEntry>,
}

impl LogFollowerState {
    async fn refresh(&mut self) -> io::Result<()> {
        use tokio::fs::File;
        use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

        let path = self.source.path();
        let mut file = match File::open(&path).await {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };

        file.seek(SeekFrom::Start(self.offset)).await?;
        let mut buffer = String::new();
        let bytes_read = file.read_to_string(&mut buffer).await?;
        if bytes_read > 0 {
            for line in buffer.lines() {
                if let Some(entry) = parse_log_line(&self.source, line) {
                    self.buffer.push_back(entry);
                }
            }
            self.offset += bytes_read as u64;
        }
        Ok(())
    }
}

fn parse_log_line(source: &LogSource, line: &str) -> Option<LogEntry> {
    let mut parts = line.splitn(3, '|');
    let timestamp = parts.next()?.trim();
    let stream = parts.next()?.trim();
    let message = parts.next().unwrap_or("").trim();
    let parsed = DateTime::parse_from_rfc3339(timestamp).ok()?;
    Some(LogEntry {
        timestamp: parsed.with_timezone(&Utc),
        target: source.target.clone(),
        generation: source.generation,
        stream: stream.to_string(),
        message: message.to_string(),
    })
}

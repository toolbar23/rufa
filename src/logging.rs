use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    sync::Arc,
};
use tracing::Level;
use tracing_subscriber::EnvFilter;

use crate::{config::model::LogConfig, paths};

static TRACING_INIT: OnceCell<()> = OnceCell::new();

pub fn init_tracing() {
    TRACING_INIT.get_or_init(|| {
        let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::builder()
                .with_default_directive(Level::WARN.into())
                .from_env_lossy()
        });

        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_target(false)
            .finish();

        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

#[derive(Debug, Clone)]
pub struct EventLogger {
    inner: Arc<Mutex<LoggerInner>>,
}

impl EventLogger {
    pub fn new(config: LogConfig) -> Result<Self> {
        let inner = LoggerInner::new(config)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    pub fn log(&self, event: LogEvent) -> Result<()> {
        let mut guard = self.inner.lock();
        guard.write_event(event)
    }

    #[allow(dead_code)]
    pub fn flush(&self) -> Result<()> {
        let mut guard = self.inner.lock();
        guard.flush()
    }
}

#[derive(Debug, Clone)]
pub struct LogEvent {
    pub target: String,
    pub generation: u64,
    pub stream: LogStream,
    pub message: Option<String>,
}

impl LogEvent {
    pub fn new(target: impl Into<String>, generation: u64, stream: LogStream) -> Self {
        Self {
            target: target.into(),
            generation,
            stream,
            message: None,
        }
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    fn format_line(&self, timestamp: &str) -> String {
        let message = self
            .message
            .as_deref()
            .map(sanitize_message)
            .unwrap_or_else(String::new);

        format!(
            "{timestamp} | {stream:<7} | {message}\n",
            timestamp = timestamp,
            stream = self.stream.label(),
            message = message
        )
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum LogStream {
    Started,
    Stdout,
    Stderr,
    Stdlog,
    Stdin,
    Exited,
}

impl LogStream {
    pub fn label(self) -> &'static str {
        match self {
            LogStream::Started => "STARTED",
            LogStream::Stdout => "STDOUT",
            LogStream::Stderr => "STDERR",
            LogStream::Stdlog => "STDLOG",
            LogStream::Stdin => "STDIN",
            LogStream::Exited => "EXITED",
        }
    }
}

#[derive(Debug)]
struct LoggerInner {
    handles: HashMap<LogKey, File>,
}

impl LoggerInner {
    fn new(config: LogConfig) -> Result<Self> {
        let _ = config;
        paths::ensure_logs_dir().with_context(|| "creating rufa log directory")?;
        Ok(Self {
            handles: HashMap::new(),
        })
    }

    fn write_event(&mut self, event: LogEvent) -> Result<()> {
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let line = event.format_line(&timestamp);
        let bytes = line.as_bytes();
        let file = self.ensure_file(&event.target, event.generation)?;
        file.write_all(bytes)
            .with_context(|| format!("writing log line for {}", event.target))?;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        for file in self.handles.values_mut() {
            file.flush().context("flushing log file")?;
        }
        Ok(())
    }

    fn ensure_file(&mut self, target: &str, generation: u64) -> Result<&mut File> {
        let key = LogKey::new(target, generation);
        if !self.handles.contains_key(&key) {
            let path = paths::log_file_path(target, generation);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating log directory {:?}", parent.display()))?;
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("opening log file {:?}", path.display()))?;
            self.handles.insert(key.clone(), file);
        }
        Ok(self.handles.get_mut(&key).expect("log handle must exist"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LogKey {
    target: String,
    generation: u64,
}

impl LogKey {
    fn new(target: &str, generation: u64) -> Self {
        Self {
            target: target.to_string(),
            generation,
        }
    }
}

fn sanitize_message(message: &str) -> String {
    message
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .collect()
}

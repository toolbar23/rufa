use anyhow::{Context, Result};
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use tracing::Level;
use tracing_subscriber::EnvFilter;

use crate::config::model::LogConfig;

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

    fn format_line(&self) -> String {
        const TARGET_FIELD: usize = 18;
        const GENERATION_FIELD: usize = 5;
        const STREAM_FIELD: usize = 7;

        let message = self
            .message
            .as_deref()
            .map(sanitize_message)
            .unwrap_or_else(String::new);

        format!(
            "{target:<target_field$} | {generation:>generation_field$} | {stream:<stream_field$} | {message}\n",
            target = self.target,
            target_field = TARGET_FIELD,
            generation = self.generation,
            generation_field = GENERATION_FIELD,
            stream = self.stream.label(),
            stream_field = STREAM_FIELD,
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
    config: LogConfig,
    file: Option<File>,
    opened_at: Instant,
    bytes_written: u64,
}

impl LoggerInner {
    fn new(config: LogConfig) -> Result<Self> {
        let mut inner = Self {
            config,
            file: None,
            opened_at: Instant::now(),
            bytes_written: 0,
        };
        inner.open_writer(true)?;
        Ok(inner)
    }

    fn write_event(&mut self, event: LogEvent) -> Result<()> {
        self.rotate_if_needed()?;
        let line = event.format_line();
        let bytes = line.as_bytes();
        let file = self.ensure_file_open()?;
        file.write_all(bytes)
            .with_context(|| format!("writing log line to {:?}", self.config.file))?;
        self.bytes_written = self.bytes_written.saturating_add(bytes.len() as u64);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.flush()
                .with_context(|| format!("flushing {:?}", self.config.file))?;
        }
        Ok(())
    }

    fn ensure_file_open(&mut self) -> Result<&mut File> {
        if self.file.is_none() {
            self.open_writer(true)?;
        }
        Ok(self
            .file
            .as_mut()
            .expect("log writer should be open after ensure_file_open"))
    }

    fn open_writer(&mut self, append: bool) -> Result<()> {
        if let Some(parent) = self.config.file.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating log directory {:?}", parent.display()))?;
            }
        }

        let mut options = OpenOptions::new();
        options.create(true).write(true);
        if append {
            options.append(true);
        } else {
            options.truncate(true);
        }

        let file = options
            .open(&self.config.file)
            .with_context(|| format!("opening log file {:?}", self.config.file))?;
        self.bytes_written = file.metadata().map(|meta| meta.len()).unwrap_or_default();
        self.opened_at = Instant::now();
        self.file = Some(file);
        Ok(())
    }

    fn rotate_if_needed(&mut self) -> Result<()> {
        if !self.should_rotate() {
            return Ok(());
        }

        if let Some(file) = self.file.as_mut() {
            file.flush()
                .with_context(|| format!("flushing {:?}", self.config.file))?;
        }

        drop(self.file.take());
        perform_rotation(&self.config.file, self.config.keep_history_count)?;
        self.open_writer(false)?;
        self.bytes_written = 0;
        Ok(())
    }

    fn should_rotate(&self) -> bool {
        self.rotation_due_to_age() || self.rotation_due_to_size()
    }

    fn rotation_due_to_age(&self) -> bool {
        self.config
            .rotation_after
            .map(|limit| self.opened_at.elapsed() >= limit)
            .unwrap_or(false)
    }

    fn rotation_due_to_size(&self) -> bool {
        self.config
            .rotation_size_bytes
            .map(|limit| self.bytes_written >= limit)
            .unwrap_or(false)
    }
}

fn perform_rotation(base: &Path, keep_history: usize) -> Result<()> {
    if keep_history == 0 {
        if base.exists() {
            fs::remove_file(base).with_context(|| format!("removing log file {:?}", base))?;
        }
        return Ok(());
    }

    for index in (1..=keep_history).rev() {
        let source = rotation_path(base, index);
        if index == keep_history {
            if source.exists() {
                fs::remove_file(&source)
                    .with_context(|| format!("removing rotated log {:?}", source.display()))?;
            }
        } else {
            let destination = rotation_path(base, index + 1);
            if source.exists() {
                fs::rename(&source, &destination).with_context(|| {
                    format!(
                        "renaming rotated log {:?} -> {:?}",
                        source.display(),
                        destination.display()
                    )
                })?;
            }
        }
    }

    if base.exists() {
        let rotated = rotation_path(base, 1);
        fs::rename(base, &rotated).with_context(|| {
            format!(
                "rotating current log {:?} -> {:?}",
                base.display(),
                rotated.display()
            )
        })?;
    }

    Ok(())
}

fn rotation_path(base: &Path, index: usize) -> PathBuf {
    if index == 0 {
        return base.to_path_buf();
    }

    let name = base
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("rufa.log");
    let rotated_name = format!("{name}.{index}");
    base.with_file_name(rotated_name)
}

fn sanitize_message(message: &str) -> String {
    message
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .collect()
}

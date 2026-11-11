//! Minimal leveled logger.
//!
//! Under systemd (`JOURNAL_STREAM` is set) every line carries a syslog
//! priority prefix so journald can color and filter it; journald timestamps
//! lines itself. The TUI installs a sink instead and renders lines itself.

use std::sync::Arc;

use chrono::{DateTime, Local};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Debug,
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug)]
pub struct LogLine {
    pub at: DateTime<Local>,
    pub level: Level,
    pub message: String,
}

pub type Sink = Arc<dyn Fn(LogLine) + Send + Sync>;

#[derive(Clone)]
pub struct Logger {
    debug: bool,
    journald: bool,
    sink: Option<Sink>,
}

impl Logger {
    pub fn stderr(debug: bool) -> Self {
        Self {
            debug,
            journald: std::env::var_os("JOURNAL_STREAM").is_some(),
            sink: None,
        }
    }

    pub fn with_sink(debug: bool, sink: Sink) -> Self {
        Self {
            debug,
            journald: false,
            sink: Some(sink),
        }
    }

    pub fn debug_enabled(&self) -> bool {
        self.debug
    }

    pub fn log(&self, level: Level, message: impl AsRef<str>) {
        if level == Level::Debug && !self.debug {
            return;
        }
        for line in message.as_ref().lines() {
            if let Some(sink) = &self.sink {
                sink(LogLine {
                    at: Local::now(),
                    level,
                    message: line.to_string(),
                });
                continue;
            }
            let (priority, label) = match level {
                Level::Debug => ("<7>", "debug: "),
                Level::Info => ("<6>", ""),
                Level::Warning => ("<4>", "warning: "),
                Level::Error => ("<3>", "error: "),
            };
            let priority = if self.journald { priority } else { "" };
            eprintln!("{priority}{label}{line}");
        }
    }

    pub fn debug(&self, message: impl AsRef<str>) {
        self.log(Level::Debug, message);
    }

    pub fn info(&self, message: impl AsRef<str>) {
        self.log(Level::Info, message);
    }

    pub fn warn(&self, message: impl AsRef<str>) {
        self.log(Level::Warning, message);
    }

    pub fn error(&self, message: impl AsRef<str>) {
        self.log(Level::Error, message);
    }
}

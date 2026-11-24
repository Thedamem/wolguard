//! Timestamped logging to stdout and/or a file, mirrored into a ring buffer
//! for the interactive dashboard.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::model::{LogBuffer, LogLevel, LogLine, SharedLogs};
use crate::util::format_datetime;

struct Inner {
    logs: SharedLogs,
    file: Mutex<Option<File>>,
    stdout: bool,
}

#[derive(Clone)]
pub struct Logger {
    inner: Arc<Inner>,
}

impl Logger {
    /// Open the configured log sink. Returns the logger and an optional warning
    /// (for example when the log file cannot be created and stdout is used).
    pub fn open(log_file: &str, to_stdout: bool, logs: SharedLogs) -> (Logger, Option<String>) {
        let mut warning = None;
        let file = if to_stdout {
            None
        } else {
            match OpenOptions::new().create(true).append(true).open(log_file) {
                Ok(file) => Some(file),
                Err(err) => {
                    warning = Some(format!(
                        "cannot open log file {log_file}: {err}; logging to stdout"
                    ));
                    None
                }
            }
        };
        let logger = Logger {
            inner: Arc::new(Inner {
                logs,
                file: Mutex::new(file),
                stdout: to_stdout || warning.is_some(),
            }),
        };
        (logger, warning)
    }

    /// File-and-buffer logger that never writes to stdout. Used by the
    /// interactive dashboard: monitor output must not be painted over the
    /// alternate screen.
    pub fn open_quiet(log_file: &str, logs: SharedLogs) -> (Logger, Option<String>) {
        let mut warning = None;
        let file = match OpenOptions::new().create(true).append(true).open(log_file) {
            Ok(file) => Some(file),
            Err(err) => {
                warning = Some(format!(
                    "cannot open log file {log_file}: {err}; dashboard log panel only"
                ));
                None
            }
        };
        let logger = Logger {
            inner: Arc::new(Inner {
                logs,
                file: Mutex::new(file),
                stdout: false,
            }),
        };
        (logger, warning)
    }

    /// Logger used for the one-shot CLI commands: stdout only, no file.
    #[cfg(test)]
    pub fn stdout_only(logs: SharedLogs) -> Logger {
        Logger {
            inner: Arc::new(Inner {
                logs,
                file: Mutex::new(None),
                stdout: true,
            }),
        }
    }

    pub fn info(&self, message: impl std::fmt::Display) {
        self.log(LogLevel::Info, message);
    }

    pub fn warn(&self, message: impl std::fmt::Display) {
        self.log(LogLevel::Warn, message);
    }

    pub fn error(&self, message: impl std::fmt::Display) {
        self.log(LogLevel::Error, message);
    }

    fn log(&self, level: LogLevel, message: impl std::fmt::Display) {
        let now = SystemTime::now();
        let text = message.to_string();
        let line = format!("{} [{}] {}", format_datetime(now), level.label(), text);

        {
            let mut buffer: std::sync::MutexGuard<'_, LogBuffer> = self
                .inner
                .logs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let seq = buffer.last_seq().saturating_add(1);
            buffer.push(LogLine {
                seq,
                at: now,
                level,
                text: text.clone(),
            });
        }

        if self.inner.stdout {
            let mut stdout = std::io::stdout();
            let _ = writeln!(stdout, "{line}");
            let _ = stdout.flush();
        }
        let mut file = self
            .inner
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(file) = file.as_mut() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LogBuffer;

    #[test]
    fn logs_are_mirrored_into_buffer() {
        let logs = Arc::new(Mutex::new(LogBuffer::new(10)));
        let logger = Logger::stdout_only(logs.clone());
        logger.info("hello");
        logger.warn("careful");
        let logs = logs.lock().unwrap();
        let lines: Vec<_> = logs.iter().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].level, LogLevel::Info);
        assert_eq!(lines[0].text, "hello");
        assert_eq!(lines[1].level, LogLevel::Warn);
    }

    #[test]
    fn assigns_monotonic_sequence_numbers_and_supports_after() {
        let logs = Arc::new(Mutex::new(LogBuffer::new(10)));
        let logger = Logger::stdout_only(logs.clone());
        logger.info("a");
        logger.info("b");
        let logs = logs.lock().unwrap();
        let lines: Vec<_> = logs.iter().collect();
        assert_eq!(lines[0].seq, 1);
        assert_eq!(lines[1].seq, 2);
        assert_eq!(logs.last_seq(), 2);
        let newer = logs.after(1, 10);
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].text, "b");
    }
}

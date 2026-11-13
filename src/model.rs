//! Shared data types between the monitor thread and the UI.

use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

pub type SharedSnapshot = Arc<Mutex<Snapshot>>;
pub type SharedLogs = Arc<Mutex<LogBuffer>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostState {
    Disabled,
    Unknown,
    Online,
    Offline,
    Waking,
}

impl HostState {
    pub fn label(self) -> &'static str {
        match self {
            HostState::Disabled => "disabled",
            HostState::Unknown => "unknown",
            HostState::Online => "online",
            HostState::Offline => "offline",
            HostState::Waking => "waking",
        }
    }
}

#[derive(Clone, Debug)]
pub struct HostStatus {
    pub name: String,
    pub ip: Ipv4Addr,
    pub mac: [u8; 6],
    pub enabled: bool,
    pub state: HostState,
    pub consecutive_failures: u32,
    pub last_rtt: Option<Duration>,
    pub last_ok: Option<SystemTime>,
    pub last_check: Option<SystemTime>,
    pub last_wake: Option<SystemTime>,
    pub wake_count: u32,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub hosts: Vec<HostStatus>,
    pub started_at: SystemTime,
    pub config_path: String,
    pub wol_port: u16,
    /// Human readable description of the wake target, e.g.
    /// "192.168.1.255 (br0)" or "configured 192.168.1.255".
    pub wake_target: String,
    pub last_update: SystemTime,
}

#[derive(Clone, Copy, Debug)]
pub enum Command {
    /// Send a Wake-on-LAN packet to the host with this snapshot index right now.
    WakeNow(usize),
    /// Enable or disable automatic waking for the host with this snapshot index.
    ToggleEnabled(usize),
    /// Stop the monitor thread.
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn label(self) -> &'static str {
        match self {
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERR ",
        }
    }
}

#[derive(Clone, Debug)]
pub struct LogLine {
    pub at: SystemTime,
    pub level: LogLevel,
    pub text: String,
}

pub struct LogBuffer {
    lines: VecDeque<LogLine>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        LogBuffer {
            lines: VecDeque::with_capacity(capacity),
            capacity: capacity.max(1),
        }
    }

    pub fn push(&mut self, line: LogLine) {
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    pub fn iter(&self) -> impl Iterator<Item = &LogLine> {
        self.lines.iter()
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }
}

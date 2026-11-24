//! Shared data types between the monitor thread and the UI.

use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::ipclass::IpClass;

pub type SharedSnapshot = Arc<Mutex<Snapshot>>;
pub type SharedLogs = Arc<Mutex<LogBuffer>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub hosts: Vec<HostStatus>,
    pub started_at: SystemTime,
    pub config_path: String,
    pub wol_port: u16,
    /// Human readable description of the wake target, e.g.
    /// "192.168.1.255 (br0)" or "configured 192.168.1.255".
    pub wake_target: String,
    pub last_update: SystemTime,
    /// Global LAN host monitoring switch.
    pub wol_enabled: bool,
    /// PPPoE interfaces and watcher state.
    pub wan: Vec<WanStatus>,
    /// Which session-change signals the firmware exposes.
    pub session_signals: Vec<String>,
}

/// One PPPoE interface as shown by the dashboard and the control protocol.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WanStatus {
    pub id: String,
    pub description: String,
    pub interface_name: String,
    pub state: String,
    pub link: String,
    pub connected: String,
    pub address: Option<Ipv4Addr>,
    pub mask: Option<Ipv4Addr>,
    pub class: Option<IpClass>,
    pub class_reason: Option<String>,
    pub uptime_secs: Option<u64>,
    /// A `[[pppoe.target]]` block exists for this interface.
    pub watched: bool,
    /// The target (and the global PPPoE switch) is active.
    pub enabled: bool,
    pub hunting: bool,
    pub phase: String,
    pub restart_count: u32,
    pub last_restart: Option<SystemTime>,
    pub next_action_in_secs: Option<u64>,
    pub last_error: Option<String>,
    /// This daemon took the interface down and must bring it back up.
    pub recovery_pending: bool,
}

#[derive(Debug)]
pub enum Command {
    /// Send a Wake-on-LAN packet to the named host right now.
    WakeNow(String),
    /// Enable or disable automatic waking for the host with this snapshot index.
    ToggleEnabled(usize),
    /// Replace the monitor configuration and rebind hosts.
    Reload {
        config: Box<Config>,
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    },
    /// Stop the monitor thread.
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogLine {
    /// Monotonic sequence assigned by the logger; the TUI polls with `after`.
    pub seq: u64,
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

    /// Sequence of the newest line, or 0 for an empty buffer.
    pub fn last_seq(&self) -> u64 {
        self.lines.back().map(|line| line.seq).unwrap_or(0)
    }

    /// Lines newer than `after`, at most `limit`, oldest first.
    pub fn after(&self, after: u64, limit: usize) -> Vec<LogLine> {
        let mut lines: Vec<LogLine> = self
            .lines
            .iter()
            .filter(|line| line.seq > after)
            .cloned()
            .collect();
        if lines.len() > limit {
            lines = lines[lines.len() - limit..].to_vec();
        }
        lines
    }
}

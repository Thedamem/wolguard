//! Configuration file handling.

use std::collections::HashSet;
use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ipclass::Classifier;

pub const DEFAULT_CONFIG_PATH: &str = "/opt/etc/wolguard.conf";
pub const DEFAULT_LOG_FILE: &str = "/opt/var/log/wolguard.log";
pub const DEFAULT_CONTROL_SOCKET: &str = "/tmp/wolguard.sock";
pub const DEFAULT_INIT_SCRIPT: &str = "/opt/etc/init.d/S99wolguard";
pub const DEFAULT_STATE_FILE: &str = "/opt/var/run/wolguard.state";

fn def_check_interval_secs() -> u64 {
    10
}

fn def_ping_timeout_ms() -> u64 {
    1000
}

fn def_failures_before_wake() -> u32 {
    3
}

fn def_wake_retry_secs() -> u64 {
    600
}

fn def_max_wakes_per_hour() -> u32 {
    6
}

fn def_enabled() -> bool {
    true
}

fn def_wol_port() -> u16 {
    9
}

fn def_log_file() -> String {
    DEFAULT_LOG_FILE.to_string()
}

fn def_pid_file() -> String {
    "/opt/var/run/wolguard.pid".to_string()
}

fn def_control_socket() -> String {
    DEFAULT_CONTROL_SOCKET.to_string()
}

fn def_init_script() -> String {
    DEFAULT_INIT_SCRIPT.to_string()
}

fn def_state_file() -> String {
    DEFAULT_STATE_FILE.to_string()
}

fn def_rci_url() -> String {
    "http://127.0.0.1:79".to_string()
}

fn def_rci_timeout_ms() -> u64 {
    2000
}

fn def_ndmc_path() -> String {
    "ndmc".to_string()
}

fn def_pppoe_poll_secs() -> u64 {
    15
}

fn def_pppoe_down_secs() -> u64 {
    5
}

fn def_pppoe_settle_secs() -> u64 {
    60
}

fn def_pppoe_retry_secs() -> u64 {
    45
}

fn def_pppoe_max_restarts_per_hour() -> u32 {
    12
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trigger {
    #[default]
    OnChange,
    Always,
}

impl Trigger {
    pub fn label(self) -> &'static str {
        match self {
            Trigger::OnChange => "on-change",
            Trigger::Always => "always",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RciConfig {
    #[serde(default = "def_rci_url")]
    pub url: String,
    /// Optional `X-NDMA-TKN` access token (KeeneticOS 5.2+).
    #[serde(default)]
    pub token: String,
    #[serde(default = "def_rci_timeout_ms")]
    pub timeout_ms: u64,
    /// Generate an access token through `ndmc` when RCI returns 401/403.
    #[serde(default = "def_enabled")]
    pub auto_token: bool,
    #[serde(default = "def_ndmc_path")]
    pub ndmc_path: String,
}

impl Default for RciConfig {
    fn default() -> Self {
        RciConfig {
            url: def_rci_url(),
            token: String::new(),
            timeout_ms: def_rci_timeout_ms(),
            auto_token: true,
            ndmc_path: def_ndmc_path(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PppoeConfig {
    /// Master switch for the PPPoE watcher.
    #[serde(default = "def_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub trigger: Trigger,
    #[serde(default = "def_pppoe_poll_secs")]
    pub poll_secs: u64,
    #[serde(default = "def_pppoe_down_secs")]
    pub down_secs: u64,
    #[serde(default = "def_pppoe_settle_secs")]
    pub settle_secs: u64,
    #[serde(default = "def_pppoe_retry_secs")]
    pub retry_secs: u64,
    #[serde(default = "def_pppoe_max_restarts_per_hour")]
    pub max_restarts_per_hour: u32,
    #[serde(default)]
    pub white_cidrs: Vec<String>,
    #[serde(default)]
    pub grey_cidrs: Vec<String>,
    #[serde(default = "Vec::new", rename = "target")]
    pub targets: Vec<PppoeTargetConfig>,
}

impl Default for PppoeConfig {
    fn default() -> Self {
        PppoeConfig {
            enabled: true,
            trigger: Trigger::OnChange,
            poll_secs: def_pppoe_poll_secs(),
            down_secs: def_pppoe_down_secs(),
            settle_secs: def_pppoe_settle_secs(),
            retry_secs: def_pppoe_retry_secs(),
            max_restarts_per_hour: def_pppoe_max_restarts_per_hour(),
            white_cidrs: Vec::new(),
            grey_cidrs: Vec::new(),
            targets: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PppoeTargetConfig {
    pub interface: String,
    #[serde(default = "def_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub trigger: Option<Trigger>,
    #[serde(default)]
    pub poll_secs: Option<u64>,
    #[serde(default)]
    pub down_secs: Option<u64>,
    #[serde(default)]
    pub settle_secs: Option<u64>,
    #[serde(default)]
    pub retry_secs: Option<u64>,
    #[serde(default)]
    pub max_restarts_per_hour: Option<u32>,
    #[serde(default)]
    pub white_cidrs: Option<Vec<String>>,
    #[serde(default)]
    pub grey_cidrs: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct PppoeTargetSettings {
    pub interface: String,
    pub enabled: bool,
    pub trigger: Trigger,
    pub poll: Duration,
    pub down: Duration,
    pub settle: Duration,
    pub retry: Duration,
    pub max_restarts_per_hour: u32,
    pub white_cidrs: Vec<String>,
    pub grey_cidrs: Vec<String>,
}

impl PppoeTargetSettings {
    pub fn classifier(&self) -> Result<Classifier, String> {
        Classifier::new(&self.white_cidrs, &self.grey_cidrs)
            .map_err(|err| format!("target {:?}: {err}", self.interface))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Global switch for LAN host monitoring. PPPoE watching is independent.
    #[serde(default = "def_enabled")]
    pub wol_enabled: bool,
    #[serde(default = "def_check_interval_secs")]
    pub check_interval_secs: u64,
    #[serde(default = "def_ping_timeout_ms")]
    pub ping_timeout_ms: u64,
    #[serde(default = "def_failures_before_wake")]
    pub failures_before_wake: u32,
    #[serde(default = "def_wake_retry_secs")]
    pub wake_retry_secs: u64,
    #[serde(default = "def_max_wakes_per_hour")]
    pub max_wakes_per_hour: u32,
    /// Wake hosts after a router reboot even if they were never seen online.
    #[serde(default = "def_enabled")]
    pub wake_on_start: bool,
    /// Broadcast address for the magic packet. "auto" derives the LAN
    /// broadcast address from the kernel routing table.
    #[serde(default = "def_broadcast")]
    pub broadcast: Option<String>,
    #[serde(default = "def_wol_port")]
    pub wol_port: u16,
    /// "-" or "" logs to stdout, otherwise a file path.
    #[serde(default = "def_log_file")]
    pub log_file: String,
    /// PID file used to keep exactly one daemon/dashboard instance running.
    #[serde(default = "def_pid_file")]
    pub pid_file: String,
    /// Unix socket used by the TUI/CLI to talk to the daemon.
    #[serde(default = "def_control_socket")]
    pub control_socket: String,
    /// Entware init script used by `wolguard service ...` and the TUI.
    #[serde(default = "def_init_script")]
    pub init_script: String,
    /// Persistent recovery/observation state.
    #[serde(default = "def_state_file")]
    pub state_file: String,
    /// Log every failed probe instead of only the first one per outage.
    #[serde(default)]
    pub verbose_probes: bool,
    #[serde(default)]
    pub rci: RciConfig,
    #[serde(default)]
    pub pppoe: PppoeConfig,
    #[serde(default = "Vec::new", rename = "host")]
    pub hosts: Vec<HostConfig>,
}

fn def_broadcast() -> Option<String> {
    Some("auto".to_string())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub name: String,
    pub ip: Ipv4Addr,
    /// "AA:BB:CC:DD:EE:FF", "aa-bb-cc-dd-ee-ff" and "aabb.ccdd.eeff" are accepted.
    pub mac: String,
    #[serde(default = "def_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub check_interval_secs: Option<u64>,
    #[serde(default)]
    pub wake_retry_secs: Option<u64>,
    #[serde(default)]
    pub failures_before_wake: Option<u32>,
    #[serde(default)]
    pub max_wakes_per_hour: Option<u32>,
}

/// Host configuration with all defaults resolved.
#[derive(Debug, Clone)]
pub struct HostSettings {
    pub name: String,
    pub ip: Ipv4Addr,
    pub mac: [u8; 6],
    pub enabled: bool,
    pub check_interval: Duration,
    pub wake_retry: Duration,
    pub failures_before_wake: u32,
    pub max_wakes_per_hour: u32,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = fs::read_to_string(path)
            .map_err(|err| format!("cannot read config {}: {err}", path.display()))?;
        Self::from_toml_str(&text)
            .map_err(|err| format!("invalid config {}: {err}", path.display()))
    }

    pub fn from_toml_str(text: &str) -> Result<Config, String> {
        let config: Config = toml::from_str(text).map_err(|err| err.to_string())?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.hosts.is_empty() && self.pppoe.targets.is_empty() {
            return Err(
                "nothing to monitor: configure at least one [[host]] or [[pppoe.target]]"
                    .to_string(),
            );
        }
        if self.check_interval_secs == 0 {
            return Err("check_interval_secs must be greater than zero".to_string());
        }
        if self.ping_timeout_ms == 0 {
            return Err("ping_timeout_ms must be greater than zero".to_string());
        }
        if self.wake_retry_secs == 0 {
            return Err("wake_retry_secs must be greater than zero".to_string());
        }
        if self.failures_before_wake == 0 {
            return Err("failures_before_wake must be greater than zero".to_string());
        }
        if self.wol_port == 0 {
            return Err("wol_port must be greater than zero".to_string());
        }
        if self.pid_file.trim().is_empty() {
            return Err("pid_file must not be empty".to_string());
        }
        if self.control_socket.trim().is_empty() {
            return Err("control_socket must not be empty".to_string());
        }
        if self.init_script.trim().is_empty() {
            return Err("init_script must not be empty".to_string());
        }
        if self.state_file.trim().is_empty() {
            return Err("state_file must not be empty".to_string());
        }
        if let Some(broadcast) = &self.broadcast {
            let broadcast = broadcast.trim();
            if !broadcast.is_empty()
                && broadcast != "auto"
                && broadcast.parse::<Ipv4Addr>().is_err()
            {
                return Err(format!(
                    "broadcast must be \"auto\" or an IPv4 address, got {broadcast:?}"
                ));
            }
        }

        self.validate_rci()?;
        self.validate_pppoe()?;

        let mut names = HashSet::new();
        for host in &self.hosts {
            if host.name.trim().is_empty() {
                return Err("host name must not be empty".to_string());
            }
            if !names.insert(host.name.clone()) {
                return Err(format!("duplicate host name {:?}", host.name));
            }
            validate_ip(host)?;
            parse_mac(&host.mac)?;
            if host.check_interval_secs == Some(0) {
                return Err(format!(
                    "host {:?}: check_interval_secs must be greater than zero",
                    host.name
                ));
            }
            if host.wake_retry_secs == Some(0) {
                return Err(format!(
                    "host {:?}: wake_retry_secs must be greater than zero",
                    host.name
                ));
            }
            if host.failures_before_wake == Some(0) {
                return Err(format!(
                    "host {:?}: failures_before_wake must be greater than zero",
                    host.name
                ));
            }
            if host.max_wakes_per_hour == Some(0) {
                return Err(format!(
                    "host {:?}: max_wakes_per_hour must be greater than zero",
                    host.name
                ));
            }
        }
        Ok(())
    }

    fn validate_rci(&self) -> Result<(), String> {
        if self.rci.timeout_ms == 0 {
            return Err("rci.timeout_ms must be greater than zero".to_string());
        }
        if self.rci.url.trim().is_empty() {
            return Err("rci.url must not be empty".to_string());
        }
        if !self.rci.url.trim().starts_with("http://") {
            return Err(format!(
                "rci.url must start with http://, got {:?}",
                self.rci.url
            ));
        }
        if self.rci.ndmc_path.trim().is_empty() {
            return Err("rci.ndmc_path must not be empty".to_string());
        }
        Ok(())
    }

    fn validate_pppoe(&self) -> Result<(), String> {
        let pppoe = &self.pppoe;
        for (name, value) in [
            ("pppoe.poll_secs", pppoe.poll_secs),
            ("pppoe.down_secs", pppoe.down_secs),
            ("pppoe.settle_secs", pppoe.settle_secs),
            ("pppoe.retry_secs", pppoe.retry_secs),
        ] {
            if value == 0 {
                return Err(format!("{name} must be greater than zero"));
            }
        }
        if pppoe.max_restarts_per_hour == 0 {
            return Err("pppoe.max_restarts_per_hour must be greater than zero".to_string());
        }
        Classifier::new(&pppoe.white_cidrs, &pppoe.grey_cidrs)
            .map_err(|err| format!("pppoe: {err}"))?;

        let mut interfaces = HashSet::new();
        for target in &pppoe.targets {
            let interface = target.interface.trim();
            if interface.is_empty() {
                return Err("pppoe target interface must not be empty".to_string());
            }
            if !interfaces.insert(interface.to_string()) {
                return Err(format!("duplicate pppoe target {:?}", interface));
            }
            for (name, value) in [
                ("poll_secs", target.poll_secs),
                ("down_secs", target.down_secs),
                ("settle_secs", target.settle_secs),
                ("retry_secs", target.retry_secs),
            ] {
                if value == Some(0) {
                    return Err(format!(
                        "pppoe target {:?}: {name} must be greater than zero",
                        interface
                    ));
                }
            }
            if target.max_restarts_per_hour == Some(0) {
                return Err(format!(
                    "pppoe target {:?}: max_restarts_per_hour must be greater than zero",
                    interface
                ));
            }
            let white = target.white_cidrs.as_deref().unwrap_or(&pppoe.white_cidrs);
            let grey = target.grey_cidrs.as_deref().unwrap_or(&pppoe.grey_cidrs);
            Classifier::new(white, grey)
                .map_err(|err| format!("pppoe target {:?}: {err}", interface))?;
        }
        Ok(())
    }

    /// Resolve per-host defaults into concrete settings.
    pub fn host_settings(&self) -> Result<Vec<HostSettings>, String> {
        self.validate()?;
        let mut result = Vec::with_capacity(self.hosts.len());
        for host in &self.hosts {
            result.push(HostSettings {
                name: host.name.trim().to_string(),
                ip: host.ip,
                mac: parse_mac(&host.mac)?,
                enabled: host.enabled && self.wol_enabled,
                check_interval: Duration::from_secs(
                    host.check_interval_secs.unwrap_or(self.check_interval_secs),
                ),
                wake_retry: Duration::from_secs(
                    host.wake_retry_secs.unwrap_or(self.wake_retry_secs),
                ),
                failures_before_wake: host
                    .failures_before_wake
                    .unwrap_or(self.failures_before_wake),
                max_wakes_per_hour: host.max_wakes_per_hour.unwrap_or(self.max_wakes_per_hour),
            });
        }
        Ok(result)
    }

    /// Resolve PPPoE targets. The global `[pppoe] enabled` switch disables all
    /// targets without deleting their rules.
    pub fn pppoe_targets(&self) -> Result<Vec<PppoeTargetSettings>, String> {
        self.validate()?;
        let pppoe = &self.pppoe;
        let mut result = Vec::with_capacity(pppoe.targets.len());
        for target in &pppoe.targets {
            result.push(PppoeTargetSettings {
                interface: target.interface.trim().to_string(),
                enabled: target.enabled && pppoe.enabled,
                trigger: target.trigger.unwrap_or(pppoe.trigger),
                poll: Duration::from_secs(target.poll_secs.unwrap_or(pppoe.poll_secs)),
                down: Duration::from_secs(target.down_secs.unwrap_or(pppoe.down_secs)),
                settle: Duration::from_secs(target.settle_secs.unwrap_or(pppoe.settle_secs)),
                retry: Duration::from_secs(target.retry_secs.unwrap_or(pppoe.retry_secs)),
                max_restarts_per_hour: target
                    .max_restarts_per_hour
                    .unwrap_or(pppoe.max_restarts_per_hour),
                white_cidrs: target
                    .white_cidrs
                    .clone()
                    .unwrap_or_else(|| pppoe.white_cidrs.clone()),
                grey_cidrs: target
                    .grey_cidrs
                    .clone()
                    .unwrap_or_else(|| pppoe.grey_cidrs.clone()),
            });
        }
        Ok(result)
    }

    pub fn ping_timeout(&self) -> Duration {
        Duration::from_millis(self.ping_timeout_ms)
    }

    pub fn logs_to_stdout(&self) -> bool {
        let path = self.log_file.trim();
        path.is_empty() || path == "-"
    }
}

fn validate_ip(host: &HostConfig) -> Result<(), String> {
    let ip = host.ip;
    if ip.is_unspecified() {
        return Err(format!(
            "host {:?}: ip must be a concrete address",
            host.name
        ));
    }
    if ip.is_multicast() {
        return Err(format!(
            "host {:?}: ip {} must not be a multicast address",
            host.name, ip
        ));
    }
    if ip.is_broadcast() {
        return Err(format!(
            "host {:?}: ip {} must not be a broadcast address",
            host.name, ip
        ));
    }
    Ok(())
}

pub fn parse_mac(input: &str) -> Result<[u8; 6], String> {
    let trimmed = input.trim();
    let hex: String = trimmed
        .chars()
        .filter(|ch| *ch != ':' && *ch != '-' && *ch != '.')
        .collect();
    if hex.len() != 12 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(format!(
            "invalid MAC address {trimmed:?} (expected AA:BB:CC:DD:EE:FF)"
        ));
    }
    let mut mac = [0u8; 6];
    for (index, byte) in mac.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|_| format!("invalid MAC address {trimmed:?}"))?;
    }
    if mac.iter().all(|byte| *byte == 0) {
        return Err(format!(
            "invalid MAC address {trimmed:?}: all bytes are zero"
        ));
    }
    if mac[0] & 0x01 != 0 {
        return Err(format!(
            "invalid MAC address {trimmed:?}: group/multicast bit is set"
        ));
    }
    Ok(mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
check_interval_secs = 10
ping_timeout_ms = 800
failures_before_wake = 3
wake_retry_secs = 600

[[host]]
name = "server"
ip = "192.168.1.10"
mac = "AA:BB:CC:DD:EE:FF"

[[host]]
name = "nas"
ip = "192.168.1.11"
mac = "aa-bb-cc-dd-ee-01"
enabled = false
check_interval_secs = 30
wake_retry_secs = 120
failures_before_wake = 2
max_wakes_per_hour = 3
"#;

    #[test]
    fn parses_sample_config_with_defaults() {
        let config = Config::from_toml_str(SAMPLE).expect("config must parse");
        assert!(config.wol_enabled);
        assert_eq!(config.check_interval_secs, 10);
        assert_eq!(config.wol_port, 9);
        assert_eq!(config.broadcast.as_deref(), Some("auto"));
        assert!(config.wake_on_start);
        assert_eq!(config.pid_file, "/opt/var/run/wolguard.pid");
        assert_eq!(config.control_socket, DEFAULT_CONTROL_SOCKET);
        assert_eq!(config.init_script, DEFAULT_INIT_SCRIPT);
        assert_eq!(config.state_file, DEFAULT_STATE_FILE);
        assert_eq!(config.rci.url, "http://127.0.0.1:79");
        assert!(config.rci.auto_token);
        assert!(!config.verbose_probes);
        assert!(!config.pppoe.enabled || config.pppoe.targets.is_empty());
        let hosts = config.host_settings().expect("hosts must resolve");
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].mac, [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        assert_eq!(hosts[0].check_interval, Duration::from_secs(10));
        assert_eq!(hosts[1].check_interval, Duration::from_secs(30));
        assert_eq!(hosts[1].wake_retry, Duration::from_secs(120));
        assert!(!hosts[1].enabled);
    }

    #[test]
    fn parses_pppoe_targets_and_overrides() {
        let text = r#"
[pppoe]
trigger = "always"
poll_secs = 20
max_restarts_per_hour = 7
grey_cidrs = ["100.64.0.0/10"]

rci_token = "not-a-field"

[[pppoe.target]]
interface = "PPPoE0"

[[pppoe.target]]
interface = "PPPoE1"
enabled = false
trigger = "on-change"
poll_secs = 40
max_restarts_per_hour = 3
"#;
        let err = Config::from_toml_str(text).expect_err("unknown top-level key must fail");
        assert!(
            err.contains("rci_token") || err.contains("unknown"),
            "{err}"
        );

        let text = r#"
[pppoe]
trigger = "always"
poll_secs = 20
max_restarts_per_hour = 7
grey_cidrs = ["100.64.0.0/10"]

[[pppoe.target]]
interface = "PPPoE0"

[[pppoe.target]]
interface = "PPPoE1"
enabled = false
trigger = "on-change"
poll_secs = 40
max_restarts_per_hour = 3
"#;
        let config = Config::from_toml_str(text).expect("pppoe-only config must parse");
        assert!(config.hosts.is_empty());
        let targets = config.pppoe_targets().expect("targets must resolve");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].interface, "PPPoE0");
        assert!(targets[0].enabled);
        assert_eq!(targets[0].trigger, Trigger::Always);
        assert_eq!(targets[0].poll, Duration::from_secs(20));
        assert_eq!(targets[0].max_restarts_per_hour, 7);
        assert_eq!(targets[1].trigger, Trigger::OnChange);
        assert_eq!(targets[1].poll, Duration::from_secs(40));
        assert!(!targets[1].enabled);
        assert!(targets[0].classifier().is_ok());
    }

    #[test]
    fn global_pppoe_switch_disables_targets_without_deleting_rules() {
        let text = r#"
[pppoe]
enabled = false
[[pppoe.target]]
interface = "PPPoE0"
"#;
        let config = Config::from_toml_str(text).expect("config must parse");
        let targets = config.pppoe_targets().expect("targets must resolve");
        assert_eq!(targets.len(), 1);
        assert!(!targets[0].enabled);
    }

    #[test]
    fn rejects_unknown_fields() {
        let text = "check_interval_sec = 5\n[[host]]\nname=\"a\"\nip=\"192.168.1.2\"\nmac=\"AA:BB:CC:DD:EE:FF\"\n";
        let err = Config::from_toml_str(text).expect_err("typo must be rejected");
        assert!(
            err.contains("check_interval_sec"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parses_verbose_probes_and_pid_file() {
        let text = r#"
pid_file = "/tmp/wolguard-test.pid"
verbose_probes = true

[[host]]
name = "server"
ip = "192.168.1.2"
mac = "AA:BB:CC:DD:EE:FF"
"#;
        let config = Config::from_toml_str(text).expect("config must parse");
        assert_eq!(config.pid_file, "/tmp/wolguard-test.pid");
        assert!(config.verbose_probes);
    }

    #[test]
    fn rejects_duplicate_host_names() {
        let text = r#"
[[host]]
name = "a"
ip = "192.168.1.2"
mac = "AA:BB:CC:DD:EE:FF"
[[host]]
name = "a"
ip = "192.168.1.3"
mac = "AA:BB:CC:DD:EE:01"
"#;
        let err = Config::from_toml_str(text).expect_err("duplicate names must be rejected");
        assert!(err.contains("duplicate"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_duplicate_pppoe_targets() {
        let text = r#"
[[pppoe.target]]
interface = "PPPoE0"
[[pppoe.target]]
interface = "PPPoE0"
"#;
        let err = Config::from_toml_str(text).expect_err("duplicate targets must be rejected");
        assert!(err.contains("duplicate"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_invalid_cidrs_and_intervals() {
        let text = r#"
[pppoe]
poll_secs = 0
[[pppoe.target]]
interface = "PPPoE0"
"#;
        let err = Config::from_toml_str(text).expect_err("zero interval must fail");
        assert!(err.contains("poll_secs"), "{err}");

        let text = r#"
[pppoe]
grey_cidrs = ["300.0.0.0/8"]
[[pppoe.target]]
interface = "PPPoE0"
"#;
        let err = Config::from_toml_str(text).expect_err("bad CIDR must fail");
        assert!(err.contains("CIDR"), "{err}");
    }

    #[test]
    fn parses_mac_formats() {
        assert_eq!(
            parse_mac("AA:BB:CC:DD:EE:FF").unwrap(),
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
        );
        assert_eq!(
            parse_mac("aa-bb-cc-dd-ee-ff").unwrap(),
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
        );
        assert_eq!(
            parse_mac("aabb.ccdd.eeff").unwrap(),
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
        );
    }

    #[test]
    fn rejects_invalid_macs() {
        assert!(parse_mac("AA:BB:CC:DD:EE").is_err());
        assert!(parse_mac("AG:BB:CC:DD:EE:FF").is_err());
        assert!(parse_mac("00:00:00:00:00:00").is_err());
        assert!(parse_mac("01:BB:CC:DD:EE:FF").is_err());
    }

    #[test]
    fn rejects_empty_config() {
        let err = Config::from_toml_str("check_interval_secs = 5").expect_err("must fail");
        assert!(
            err.contains("host") || err.contains("pppoe"),
            "unexpected error: {err}"
        );
    }
}

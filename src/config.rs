//! Configuration file handling.

use std::collections::HashSet;
use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

pub const DEFAULT_CONFIG_PATH: &str = "/opt/etc/wolguard.conf";
pub const DEFAULT_LOG_FILE: &str = "/opt/var/log/wolguard.log";

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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
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
    /// Log every failed probe instead of only the first one per outage.
    #[serde(default)]
    pub verbose_probes: bool,
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
        if self.hosts.is_empty() {
            return Err("no [[host]] sections configured".to_string());
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

    /// Resolve per-host defaults into concrete settings.
    pub fn host_settings(&self) -> Result<Vec<HostSettings>, String> {
        self.validate()?;
        let mut result = Vec::with_capacity(self.hosts.len());
        for host in &self.hosts {
            result.push(HostSettings {
                name: host.name.trim().to_string(),
                ip: host.ip,
                mac: parse_mac(&host.mac)?,
                enabled: host.enabled,
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
        assert_eq!(config.check_interval_secs, 10);
        assert_eq!(config.wol_port, 9);
        assert_eq!(config.broadcast.as_deref(), Some("auto"));
        assert!(config.wake_on_start);
        assert_eq!(config.pid_file, "/opt/var/run/wolguard.pid");
        assert!(!config.verbose_probes);
        let hosts = config.host_settings().expect("hosts must resolve");
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].mac, [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        assert_eq!(hosts[0].check_interval, Duration::from_secs(10));
        assert_eq!(hosts[1].check_interval, Duration::from_secs(30));
        assert_eq!(hosts[1].wake_retry, Duration::from_secs(120));
        assert!(!hosts[1].enabled);
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
    fn rejects_empty_host_list() {
        let err = Config::from_toml_str("check_interval_secs = 5").expect_err("must fail");
        assert!(err.contains("host"), "unexpected error: {err}");
    }
}

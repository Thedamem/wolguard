//! wolguard - ping hosts and wake them with Wake-on-LAN.
//!
//! Designed for Keenetic routers running Entware: a small static binary that
//! checks a list of LAN hosts with ICMP, sends a magic packet to hosts that do
//! not answer, and optionally shows a ratatui dashboard over SSH.

mod config;
mod log;
mod model;
mod monitor;
mod net;
mod ping;
mod term;
mod tui;
mod util;
mod wol;

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::config::{Config, HostSettings};
use crate::log::Logger;
use crate::model::{Command, LogBuffer, SharedSnapshot, Snapshot};
use crate::ping::{PingOutcome, Pinger};
use crate::util::{format_mac, format_millis};

static STOP: AtomicBool = AtomicBool::new(false);

pub fn stop_requested() -> bool {
    STOP.load(Ordering::Relaxed)
}

extern "C" fn handle_signal(_signal: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            handle_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        // The daemon is started over SSH or by the Entware rc scripts; neither
        // must kill it when the session goes away.
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

#[derive(Parser)]
#[command(
    name = "wolguard",
    version,
    about = "Ping LAN hosts and wake them with Wake-on-LAN"
)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(
        short,
        long,
        global = true,
        value_name = "PATH",
        default_value = config::DEFAULT_CONFIG_PATH
    )]
    config: PathBuf,

    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Run the monitor loop in the foreground (used by the init script).
    Run,
    /// Show the interactive dashboard (requires a terminal over SSH).
    Tui,
    /// Print a one-shot status snapshot (works without a TTY, e.g. in the web console).
    Status,
    /// Send one Wake-on-LAN packet and exit.
    Wake {
        /// Host name, IP address or 1-based index from the configuration.
        host: String,
    },
    /// Validate the configuration and print the resolved settings.
    CheckConfig,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        CliCommand::Run => run_daemon(&cli.config),
        CliCommand::Tui => run_tui(&cli.config),
        CliCommand::Status => run_status(&cli.config),
        CliCommand::Wake { host } => run_wake(&cli.config, &host),
        CliCommand::CheckConfig => run_check_config(&cli.config),
    };
    if let Err(err) = result {
        eprintln!("wolguard: {err}");
        std::process::exit(1);
    }
}

fn load_config(path: &Path) -> Result<(Config, Vec<HostSettings>), String> {
    let config = Config::load(path)?;
    let hosts = config.host_settings()?;
    Ok((config, hosts))
}

fn build_logger(config: &Config) -> (Logger, crate::model::SharedLogs) {
    let logs = Arc::new(Mutex::new(LogBuffer::new(500)));
    let (logger, warning) = Logger::open(&config.log_file, config.logs_to_stdout(), logs.clone());
    if let Some(warning) = warning {
        logger.warn(warning);
    }
    (logger, logs)
}

/// Dashboard mode: log lines go to the file (if it can be opened) and the
/// in-memory buffer only, never to stdout, otherwise they would be painted
/// over the alternate screen.
fn build_quiet_logger(config: &Config) -> (Logger, crate::model::SharedLogs) {
    let logs = Arc::new(Mutex::new(LogBuffer::new(500)));
    let (logger, warning) = Logger::open_quiet(&config.log_file, logs.clone());
    if let Some(warning) = warning {
        logger.warn(warning);
    }
    (logger, logs)
}

fn initial_snapshot(hosts: &[HostSettings], config: &Config, path: &Path) -> SharedSnapshot {
    let snapshot: Snapshot = monitor::initial_snapshot(hosts, config, path.display().to_string());
    Arc::new(Mutex::new(snapshot))
}

fn run_daemon(path: &Path) -> Result<(), String> {
    let (config, hosts) = load_config(path)?;
    let _instance = InstanceLock::acquire(Path::new(&config.pid_file))?;
    let (logger, _logs) = build_logger(&config);
    install_signal_handlers();
    let snapshot = initial_snapshot(&hosts, &config, path);
    let (sender, receiver) = mpsc::channel::<Command>();
    let monitor = monitor::spawn(config.clone(), logger.clone(), snapshot, receiver);

    logger.info(format!(
        "wolguard {} started (pid {})",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    ));
    while !stop_requested() {
        thread::sleep(Duration::from_millis(100));
    }
    logger.info("stop signal received");
    let _ = sender.send(Command::Shutdown);
    let _ = monitor.join();
    Ok(())
}

fn run_tui(path: &Path) -> Result<(), String> {
    let (config, hosts) = load_config(path)?;
    let _instance = InstanceLock::acquire(Path::new(&config.pid_file))?;
    let (logger, logs) = build_quiet_logger(&config);
    install_signal_handlers();
    let snapshot = initial_snapshot(&hosts, &config, path);
    let (sender, receiver) = mpsc::channel::<Command>();
    let monitor = monitor::spawn(config.clone(), logger.clone(), snapshot.clone(), receiver);

    let result = tui::run(snapshot, logs, sender.clone());

    let _ = sender.send(Command::Shutdown);
    let _ = monitor.join();
    result.map_err(|err| format!("interactive dashboard failed: {err}"))
}

fn run_wake(path: &Path, selector: &str) -> Result<(), String> {
    let (config, hosts) = load_config(path)?;
    let host = find_host(&hosts, selector)?;
    let target = wol::resolve_wake_target(host.ip, config.broadcast.as_deref());
    println!(
        "wake target for {} ({}): {} (udp {})",
        host.name,
        format_mac(host.mac),
        target,
        config.wol_port
    );
    wol::send_magic(
        host.mac,
        target.address,
        target.iface.as_deref(),
        config.wol_port,
    )
    .map_err(|err| format!("cannot send Wake-on-LAN: {err}"))?;
    println!("wake packet sent");
    Ok(())
}

fn find_host<'a>(hosts: &'a [HostSettings], selector: &str) -> Result<&'a HostSettings, String> {
    let selector = selector.trim();
    if let Some(host) = hosts
        .iter()
        .find(|host| host.name.eq_ignore_ascii_case(selector))
    {
        return Ok(host);
    }
    if let Ok(ip) = selector.parse::<Ipv4Addr>() {
        if let Some(host) = hosts.iter().find(|host| host.ip == ip) {
            return Ok(host);
        }
    }
    if let Ok(index) = selector.parse::<usize>() {
        if index >= 1 && index <= hosts.len() {
            return Ok(&hosts[index - 1]);
        }
    }
    let available: Vec<String> = hosts
        .iter()
        .map(|host| format!("{} ({})", host.name, host.ip))
        .collect();
    Err(format!(
        "host {selector:?} not found; configured hosts: {}",
        available.join(", ")
    ))
}

fn run_check_config(path: &Path) -> Result<(), String> {
    let (config, hosts) = load_config(path)?;
    println!("configuration: {}", path.display());
    println!(
        "  log file: {}",
        if config.logs_to_stdout() {
            "stdout".to_string()
        } else {
            config.log_file.clone()
        }
    );
    println!("  pid file: {}", config.pid_file);
    println!(
        "  defaults: check every {} s, ping timeout {} ms, wake after {} failures, retry {} s",
        config.check_interval_secs,
        config.ping_timeout_ms,
        config.failures_before_wake,
        config.wake_retry_secs
    );
    println!(
        "  Wake-on-LAN: udp port {}, broadcast {}",
        config.wol_port,
        config.broadcast.as_deref().unwrap_or("auto")
    );
    println!("hosts: {}", hosts.len());
    for (index, host) in hosts.iter().enumerate() {
        let target = wol::resolve_wake_target(host.ip, config.broadcast.as_deref());
        println!(
            "  {}. {} ({}) mac {}",
            index + 1,
            host.name,
            host.ip,
            format_mac(host.mac)
        );
        println!(
            "     {}, wake target {}, ping {} ms every {} s",
            if host.enabled { "enabled" } else { "disabled" },
            target,
            config.ping_timeout_ms,
            host.check_interval.as_secs()
        );
        if target.iface.is_none() && !target.configured {
            println!(
                "     note: could not derive the LAN broadcast address from the routing table; \
                 set broadcast explicitly if wake-ups fail"
            );
        }
        println!(
            "     wake after {} failed checks, retry every {} s, at most {}/hour",
            host.failures_before_wake,
            host.wake_retry.as_secs(),
            host.max_wakes_per_hour
        );
        println!(
            "     first wake attempt after ~{} s of downtime",
            host.failures_before_wake as u64 * host.check_interval.as_secs()
        );
    }
    println!(
        "Wake-on-LAN packet: 102 bytes (6 x ff + 16 x MAC), udp port {}",
        config.wol_port
    );
    println!("configuration OK");
    Ok(())
}

/// One-shot status snapshot for terminals that cannot host the dashboard, for
/// example the Keenetic web console, which does not provide a PTY.
fn run_status(path: &Path) -> Result<(), String> {
    let (config, hosts) = load_config(path)?;
    println!("wolguard {} status", env!("CARGO_PKG_VERSION"));
    println!("config: {}", path.display());
    println!(
        "daemon: {}",
        match running_instance_pid(Path::new(&config.pid_file)) {
            Some(pid) => format!("running (pid {pid})"),
            None => "not running".to_string(),
        }
    );
    println!(
        "log: {}",
        if config.logs_to_stdout() {
            "stdout".to_string()
        } else {
            config.log_file.clone()
        }
    );
    println!(
        "wake policy: after {} failed checks, retry every {} s, at most {}/hour",
        config.failures_before_wake, config.wake_retry_secs, config.max_wakes_per_hour
    );

    let mut pinger = match Pinger::new() {
        Ok(pinger) => Some(pinger),
        Err(err) => {
            println!("probe: unavailable ({err}); raw ICMP requires root");
            None
        }
    };
    println!("hosts: {}", hosts.len());
    for (index, host) in hosts.iter().enumerate() {
        let state = match pinger.as_mut() {
            Some(pinger) => match pinger.ping(host.ip, config.ping_timeout()) {
                PingOutcome::Reply(rtt) => format!("online, rtt {}", format_millis(rtt)),
                PingOutcome::Timeout => "offline (no reply)".to_string(),
                PingOutcome::Error(err) => format!("probe error: {err}"),
            },
            None => "unknown".to_string(),
        };
        let target = wol::resolve_wake_target(host.ip, config.broadcast.as_deref());
        println!(
            "  {}. {} ({}) mac {} - {}",
            index + 1,
            host.name,
            host.ip,
            format_mac(host.mac),
            state
        );
        println!(
            "     {}, wake target {}, ping every {} s, first wake after ~{} s",
            if host.enabled { "enabled" } else { "disabled" },
            target,
            host.check_interval.as_secs(),
            host.failures_before_wake as u64 * host.check_interval.as_secs()
        );
    }
    Ok(())
}

/// Keeps exactly one daemon/dashboard instance alive and records its PID.
struct InstanceLock {
    path: PathBuf,
}

impl InstanceLock {
    fn acquire(path: &Path) -> Result<InstanceLock, String> {
        if let Some(pid) = running_instance_pid(path) {
            return Err(format!(
                "another wolguard instance is already running (pid {pid}); \
                 stop it first: /opt/etc/init.d/S99wolguard stop"
            ));
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        std::fs::write(path, format!("{}\n", std::process::id()))
            .map_err(|err| format!("cannot write pid file {}: {err}", path.display()))?;
        Ok(InstanceLock {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// PID from `path` when that process really is a wolguard instance.
fn running_instance_pid(path: &Path) -> Option<u32> {
    let pid = std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    if process_is_wolguard(pid) {
        Some(pid)
    } else {
        None
    }
}

fn process_is_wolguard(pid: u32) -> bool {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
    if comm.trim() == "wolguard" {
        return true;
    }
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let first = cmdline.split(|byte| *byte == 0).next().unwrap_or(&[]);
    let name = Path::new(std::str::from_utf8(first).unwrap_or(""))
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    name.starts_with("wolguard")
}

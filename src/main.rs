//! wolguard - ping hosts and wake them with Wake-on-LAN, and keep a Keenetic
//! PPPoE interface on a white IPv4 address. One daemon, a control socket and
//! a TUI/CLI management console for Entware routers.

mod config;
mod configedit;
mod control;
mod ipclass;
mod log;
mod model;
mod monitor;
mod net;
mod ping;
mod pppoe;
mod prompt;
mod rci;
mod statefile;
mod style;
mod term;
mod tui;
mod util;
mod wan;
mod wol;

use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{json, Value};

use crate::config::{Config, HostSettings, PppoeTargetSettings, Trigger};
use crate::configedit::{ConfigDoc, HostEdit, PppoeDefaults, RciEdit, TargetEdit};
use crate::control::{Control, ControlClient, Request};
use crate::ipclass::{Classifier, IpClass};
use crate::log::Logger;
use crate::model::{Command, HostState, LogBuffer, SharedSnapshot, Snapshot, WanStatus};
use crate::ping::{PingOutcome, Pinger};
use crate::pppoe::Phase;
use crate::prompt as ask;
use crate::rci::{DhcpBinding, RciClient, RciError};
use crate::statefile::StateFile;
use crate::style::Style;
use crate::util::{
    format_age, format_hms, format_mac, format_millis, format_seconds, format_uptime,
};
use crate::wan::WanCommand;

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
    about = "Wake-on-LAN guard and PPPoE white-IP watchdog for Keenetic/Entware",
    arg_required_else_help = false,
    after_help = "Run `wolguard` with no arguments for the dashboard on a TTY.\n\
                  Run `wolguard setup` for an interactive first-run wizard.\n\
                  Run `wolguard doctor` to diagnose RCI, ICMP and service issues."
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

    /// Plain ASCII output (no colours, no Unicode box drawing).
    #[arg(long, global = true)]
    ascii: bool,

    /// Machine-readable JSON where supported.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Run the monitor loop in the foreground (used by the init script).
    Run,
    /// Attach the interactive dashboard to the running daemon.
    Tui {
        /// Development mode: run an in-process monitor without a daemon.
        #[arg(long)]
        standalone: bool,
    },
    /// Print a one-shot status snapshot (works without a TTY).
    Status,
    /// Interactive first-run wizard.
    Setup,
    /// Manage LAN hosts.
    #[command(alias = "host")]
    Hosts {
        #[command(subcommand)]
        command: Option<HostsCommand>,
    },
    /// Send one Wake-on-LAN packet and exit.
    Wake {
        /// Host name, IP address or 1-based index from the configuration.
        host: String,
    },
    /// Scan, select and control PPPoE interfaces.
    Pppoe {
        #[command(subcommand)]
        command: Option<PppoeCommand>,
    },
    /// Show or change global settings.
    Settings {
        #[command(subcommand)]
        command: Option<SettingsCommand>,
    },
    /// Inspect or validate the configuration file.
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    /// Show or follow the daemon log.
    Logs {
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        #[arg(short, long)]
        follow: bool,
    },
    /// Control the Entware service.
    Service {
        #[arg(value_enum)]
        action: ServiceAction,
    },
    /// Diagnose configuration, RCI, ICMP and service problems.
    Doctor,
    /// RCI access token helpers.
    Rci {
        #[command(subcommand)]
        command: RciCommand,
    },
    /// Validate the configuration and print the resolved settings.
    #[command(alias = "check_config")]
    CheckConfig,
}

#[derive(Subcommand)]
enum HostsCommand {
    /// List configured hosts.
    List,
    /// Add a host (interactive on a TTY, flags for scripts).
    Add {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        ip: Option<String>,
        #[arg(long)]
        mac: Option<String>,
        #[arg(long)]
        interval: Option<u64>,
        #[arg(long)]
        wake_retry: Option<u64>,
        #[arg(long)]
        failures: Option<u32>,
        #[arg(long)]
        max_wakes: Option<u32>,
        #[arg(long)]
        disabled: bool,
    },
    /// Edit a host.
    Edit {
        name: String,
        #[arg(long)]
        new_name: Option<String>,
        #[arg(long)]
        ip: Option<String>,
        #[arg(long)]
        mac: Option<String>,
        #[arg(long)]
        interval: Option<u64>,
        #[arg(long)]
        wake_retry: Option<u64>,
        #[arg(long)]
        failures: Option<u32>,
        #[arg(long)]
        max_wakes: Option<u32>,
        #[arg(long)]
        disabled: bool,
    },
    /// Remove a host.
    Remove {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    /// Enable monitoring for a host.
    Enable { name: String },
    /// Disable monitoring for a host.
    Disable { name: String },
    /// Wake a host now.
    Wake { name: String },
    /// List DHCP lease bindings reported by RCI (useful for `hosts add`).
    Leases,
}

#[derive(Subcommand)]
enum PppoeCommand {
    /// List PPPoE interfaces (default).
    List,
    /// Force a fresh scan and list PPPoE interfaces.
    Scan,
    /// Show one interface in detail.
    Show { interface: String },
    /// Restart one interface (one down/up cycle, through the daemon).
    Restart {
        interface: String,
        #[arg(long)]
        force: bool,
    },
    /// Test the watchdog manually (one cycle, or classify only with --dry-run).
    Test {
        interface: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        force: bool,
    },
    /// Watch an interface (create/enable a target without touching its rules).
    Watch { interface: String },
    /// Stop watching an interface, keeping its rules.
    Unwatch { interface: String },
    /// Remove a target block and its rules.
    Remove {
        interface: String,
        #[arg(long)]
        yes: bool,
    },
    /// Show or edit the rules of a target.
    Rules {
        interface: Option<String>,
        #[arg(long)]
        edit: bool,
    },
}

#[derive(Subcommand)]
enum SettingsCommand {
    Show,
    Set { key: String, value: String },
    Edit,
}

#[derive(Subcommand)]
enum ConfigCommand {
    Show,
    Validate,
    Path,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ServiceAction {
    Start,
    Stop,
    Restart,
    Status,
}

impl ServiceAction {
    fn as_str(self) -> &'static str {
        match self {
            ServiceAction::Start => "start",
            ServiceAction::Stop => "stop",
            ServiceAction::Restart => "restart",
            ServiceAction::Status => "status",
        }
    }
}

#[derive(Subcommand)]
enum RciCommand {
    /// Generate a Keenetic RCI access token via ndmc and save it.
    Token,
    /// Show the RCI URL and whether a token is configured.
    Show,
}

fn main() {
    let cli = Cli::parse();
    let style = Style::detect().with_ascii(cli.ascii);
    let result = match cli.command {
        None => {
            if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                run_tui(&cli.config, false)
            } else {
                run_status(&cli.config, style, cli.json)
            }
        }
        Some(CliCommand::Run) => run_daemon(&cli.config),
        Some(CliCommand::Tui { standalone }) => run_tui(&cli.config, standalone),
        Some(CliCommand::Status) => run_status(&cli.config, style, cli.json),
        Some(CliCommand::Setup) => run_setup(&cli.config, style),
        Some(CliCommand::Hosts { command }) => run_hosts(&cli.config, command, style, cli.json),
        Some(CliCommand::Wake { host }) => run_wake(&cli.config, &host),
        Some(CliCommand::Pppoe { command }) => run_pppoe(&cli.config, command, style, cli.json),
        Some(CliCommand::Settings { command }) => {
            run_settings(&cli.config, command, style, cli.json)
        }
        Some(CliCommand::Config { command }) => run_config(&cli.config, command, style, cli.json),
        Some(CliCommand::Logs { lines, follow }) => run_logs(&cli.config, lines, follow, style),
        Some(CliCommand::Service { action }) => run_service(&cli.config, action, style),
        Some(CliCommand::Doctor) => run_doctor(&cli.config, style),
        Some(CliCommand::Rci { command }) => run_rci(&cli.config, command, style),
        Some(CliCommand::CheckConfig) => run_check_config(&cli.config, style),
    };
    if let Err(err) = result {
        eprintln!("{} {err}", style.red("wolguard:"));
        std::process::exit(1);
    }
}

fn load_config(path: &Path) -> Result<(Config, Vec<HostSettings>), String> {
    let config = Config::load(path)?;
    let hosts = config.host_settings()?;
    Ok((config, hosts))
}

fn build_logger(config: &Config) -> (Logger, crate::model::SharedLogs) {
    let logs = Arc::new(Mutex::new(LogBuffer::new(1000)));
    let (logger, warning) = Logger::open(&config.log_file, config.logs_to_stdout(), logs.clone());
    if let Some(warning) = warning {
        logger.warn(warning);
    }
    (logger, logs)
}

fn build_quiet_logger(config: &Config) -> (Logger, crate::model::SharedLogs) {
    let logs = Arc::new(Mutex::new(LogBuffer::new(1000)));
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
    let _instance = match InstanceLock::acquire(Path::new(&config.pid_file)) {
        Ok(lock) => lock,
        Err(LockError::AlreadyRunning(pid)) => {
            println!(
                "wolguard is already running{}; use `wolguard status`, `wolguard tui` or \
                 `wolguard pppoe` to interact with it",
                if pid > 0 {
                    format!(" (pid {pid})")
                } else {
                    String::new()
                }
            );
            return Ok(());
        }
        Err(LockError::Io(err)) => return Err(err),
    };
    let (logger, logs) = build_logger(&config);
    install_signal_handlers();
    let snapshot = initial_snapshot(&hosts, &config, path);

    let (host_tx, host_rx) = mpsc::channel::<Command>();
    let monitor = monitor::spawn(config.clone(), logger.clone(), snapshot.clone(), host_rx);
    let (wan_tx, wan_rx) = mpsc::channel::<WanCommand>();
    let wan = wan::spawn(
        config.clone(),
        logger.clone(),
        snapshot.clone(),
        path.to_path_buf(),
        wan_rx,
    );

    let control = Arc::new(
        Control::new(
            snapshot.clone(),
            logs,
            host_tx.clone(),
            wan_tx.clone(),
            config.clone(),
        )
        .with_config_path(path.to_path_buf()),
    );
    let server = match control::spawn_server(Path::new(&config.control_socket), control) {
        Ok(server) => server,
        Err(err) => {
            let _ = host_tx.send(Command::Shutdown);
            let _ = wan_tx.send(WanCommand::Shutdown);
            let _ = monitor.join();
            let _ = wan.join();
            return Err(err);
        }
    };

    logger.info(format!(
        "wolguard {} started (pid {}), control socket {}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        config.control_socket
    ));
    while !stop_requested() {
        thread::sleep(Duration::from_millis(100));
    }
    logger.info("stop signal received");
    let _ = host_tx.send(Command::Shutdown);
    let _ = wan_tx.send(WanCommand::Shutdown);
    let _ = monitor.join();
    let _ = wan.join();
    drop(server);
    Ok(())
}

fn run_tui(path: &Path, standalone: bool) -> Result<(), String> {
    if standalone {
        let (config, hosts) = load_config(path)?;
        let _instance = match InstanceLock::acquire(Path::new(&config.pid_file)) {
            Ok(lock) => lock,
            Err(LockError::AlreadyRunning(pid)) => {
                return Err(format!(
                    "a wolguard daemon is already running (pid {pid}); use `wolguard tui` \
                     to attach to it"
                ))
            }
            Err(LockError::Io(err)) => return Err(err),
        };
        let (logger, logs) = build_quiet_logger(&config);
        install_signal_handlers();
        let snapshot = initial_snapshot(&hosts, &config, path);
        let (sender, receiver) = mpsc::channel::<Command>();
        let monitor = monitor::spawn(config.clone(), logger, snapshot.clone(), receiver);
        let result = tui::run(snapshot, logs, sender.clone());
        let _ = sender.send(Command::Shutdown);
        let _ = monitor.join();
        return result.map_err(|err| format!("interactive dashboard failed: {err}"));
    }
    let config = Config::load(path)?;
    tui::run_client(Path::new(&config.control_socket), path)
}

fn control_client(config: &Config) -> Option<ControlClient> {
    let client = ControlClient::connect(&config.control_socket).ok()?;
    client.ping().ok()?;
    Some(client)
}

fn daemon_data(client: &ControlClient) -> Result<Value, String> {
    client.status()
}

fn snapshot_from_data(data: &Value) -> Result<Snapshot, String> {
    let value = data
        .get("snapshot")
        .ok_or_else(|| "daemon status has no snapshot".to_string())?;
    serde_json::from_value(value.clone()).map_err(|err| format!("invalid daemon snapshot: {err}"))
}

fn run_status(path: &Path, style: Style, json: bool) -> Result<(), String> {
    let (config, hosts) = load_config(path)?;
    if let Some(client) = control_client(&config) {
        if let Ok(data) = daemon_data(&client) {
            return print_daemon_status(&data, &config, style, json);
        }
    }

    let wan = if config.pppoe.targets.is_empty() {
        Vec::new()
    } else {
        direct_wan_statuses(&config).unwrap_or_else(|err| {
            eprintln!("{} {err}", style.yellow("wolguard:"));
            Vec::new()
        })
    };
    if json {
        let mut pinger = Pinger::new().ok();
        let host_values: Vec<Value> = hosts
            .iter()
            .map(|host| {
                let (state, rtt_ms) = if !config.wol_enabled || !host.enabled {
                    ("disabled", None)
                } else {
                    match pinger.as_mut() {
                        Some(pinger) => match pinger.ping(host.ip, config.ping_timeout()) {
                            PingOutcome::Reply(rtt) => ("online", Some(rtt.as_millis() as u64)),
                            PingOutcome::Timeout => ("offline", None),
                            PingOutcome::Error(_) => ("error", None),
                        },
                        None => ("unknown", None),
                    }
                };
                json!({
                    "name": host.name,
                    "ip": host.ip.to_string(),
                    "mac": format_mac(host.mac),
                    "enabled": host.enabled && config.wol_enabled,
                    "state": state,
                    "rtt_ms": rtt_ms,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "daemon": false,
                "config_path": path.display().to_string(),
                "wol_enabled": config.wol_enabled,
                "hosts": host_values,
                "wan": wan,
            }))
            .map_err(|err| err.to_string())?
        );
        return Ok(());
    }

    println!(
        "{} {} status",
        style.bold("wolguard"),
        env!("CARGO_PKG_VERSION")
    );
    println!("{} {}", style.dim("config:"), path.display());
    println!("{} {}", style.dim("daemon:"), style.yellow("not running"));
    println!(
        "{} {}",
        style.dim("log:"),
        if config.logs_to_stdout() {
            "stdout".to_string()
        } else {
            config.log_file.clone()
        }
    );
    println!(
        "{} {}",
        style.dim("WOL:"),
        if config.wol_enabled {
            style.green("enabled")
        } else {
            style.yellow("disabled globally")
        }
    );
    println!(
        "{} after {} failed checks, retry every {} s, at most {}/hour",
        style.dim("wake policy:"),
        config.failures_before_wake,
        config.wake_retry_secs,
        config.max_wakes_per_hour
    );

    let mut pinger = Pinger::new().ok();
    if pinger.is_none() {
        println!(
            "{} raw ICMP is unavailable (run as root)",
            style.yellow("probe:")
        );
    }
    if !hosts.is_empty() {
        let mut rows = Vec::new();
        for (index, host) in hosts.iter().enumerate() {
            let state = if !config.wol_enabled || !host.enabled {
                "disabled".to_string()
            } else {
                match pinger.as_mut() {
                    Some(pinger) => match pinger.ping(host.ip, config.ping_timeout()) {
                        PingOutcome::Reply(rtt) => {
                            style.green(format!("online {}", format_millis(rtt)))
                        }
                        PingOutcome::Timeout => style.red("offline"),
                        PingOutcome::Error(err) => style.yellow(format!("probe: {err}")),
                    },
                    None => style.dim("unknown").to_string(),
                }
            };
            rows.push(vec![
                (index + 1).to_string(),
                host.name.clone(),
                host.ip.to_string(),
                format_mac(host.mac),
                state,
            ]);
        }
        println!("\n{}", style.heading(&format!("hosts ({})", hosts.len())));
        print!(
            "{}",
            style.table(
                &["#", "Name", "IP", "MAC", "State"],
                &rows,
                &[true, false, false, false, false]
            )
        );
    }
    if !wan.is_empty() {
        println!("\n{}", style.heading("PPPoE interfaces"));
        print_wan_table(&wan, style);
    } else if !config.pppoe.targets.is_empty() {
        println!(
            "\n{} PPPoE interfaces unavailable (RCI at {})",
            style.yellow("note:"),
            config.rci.url
        );
    }
    Ok(())
}

fn print_daemon_status(
    data: &Value,
    config: &Config,
    style: Style,
    json: bool,
) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(data).map_err(|err| err.to_string())?
        );
        return Ok(());
    }
    let snapshot = snapshot_from_data(data)?;
    println!(
        "{} {} status",
        style.bold("wolguard"),
        env!("CARGO_PKG_VERSION")
    );
    println!("{} {}", style.dim("config:"), snapshot.config_path);
    println!(
        "{} running (pid {}), control {}",
        style.dim("daemon:"),
        data.get("daemon_pid").and_then(Value::as_u64).unwrap_or(0),
        config.control_socket
    );
    println!(
        "{} {}",
        style.dim("uptime:"),
        format_uptime(snapshot.started_at, SystemTime::now())
    );
    println!(
        "{} {}",
        style.dim("WOL:"),
        if snapshot.wol_enabled {
            style.green("enabled")
        } else {
            style.yellow("disabled globally")
        }
    );
    println!(
        "{} after {} failed checks, retry every {} s, at most {}/hour",
        style.dim("wake policy:"),
        config.failures_before_wake,
        config.wake_retry_secs,
        config.max_wakes_per_hour
    );
    println!("{} {}", style.dim("wake target:"), snapshot.wake_target);
    if !snapshot.session_signals.is_empty() {
        println!(
            "{} {}",
            style.dim("session signals:"),
            snapshot.session_signals.join(", ")
        );
    }

    if !snapshot.hosts.is_empty() {
        let now = SystemTime::now();
        let rows: Vec<Vec<String>> = snapshot
            .hosts
            .iter()
            .enumerate()
            .map(|(index, host)| {
                vec![
                    (index + 1).to_string(),
                    host.name.clone(),
                    host.ip.to_string(),
                    format_mac(host.mac),
                    style
                        .paint(host.state.label(), host_state_color(host.state))
                        .to_string(),
                    host.last_rtt
                        .map(format_millis)
                        .unwrap_or_else(|| "-".to_string()),
                    format_age(now, host.last_ok),
                    host.wake_count.to_string(),
                ]
            })
            .collect();
        println!(
            "\n{}",
            style.heading(&format!("hosts ({})", snapshot.hosts.len()))
        );
        print!(
            "{}",
            style.table(
                &[
                    "#",
                    "Name",
                    "IP",
                    "MAC",
                    "State",
                    "RTT",
                    "Last seen",
                    "Wakes"
                ],
                &rows,
                &[true, false, false, false, false, true, false, true]
            )
        );
    }
    if !snapshot.wan.is_empty() {
        println!(
            "\n{}",
            style.heading(&format!("PPPoE interfaces ({})", snapshot.wan.len()))
        );
        print_wan_table(&snapshot.wan, style);
    } else if !config.pppoe.targets.is_empty() {
        println!("\n{}", style.yellow("PPPoE: no interfaces reported yet"));
    }
    Ok(())
}

fn host_state_color(state: HostState) -> &'static str {
    match state {
        HostState::Online => "32",
        HostState::Offline => "31",
        HostState::Waking => "33",
        HostState::Unknown => "2",
        HostState::Disabled => "2",
    }
}

fn direct_wan_statuses(config: &Config) -> Result<Vec<WanStatus>, String> {
    let mut client = RciClient::from_url(
        &config.rci.url,
        &config.rci.token,
        Duration::from_millis(config.rci.timeout_ms),
    )?;
    let interfaces = client.list_pppoe().map_err(|err| err.to_string())?;
    let classifier = Classifier::new(&config.pppoe.white_cidrs, &config.pppoe.grey_cidrs)?;
    let targets = config.pppoe_targets()?;
    let state = StateFile::load(Path::new(&config.state_file)).unwrap_or_default();
    let mut statuses = Vec::new();
    for interface in &interfaces {
        let target = targets
            .iter()
            .find(|target| target.interface == interface.id);
        statuses.push(wan_status_from_interface(
            interface,
            target,
            &classifier,
            &state,
        ));
    }
    for target in &targets {
        if interfaces
            .iter()
            .any(|interface| interface.id == target.interface)
        {
            continue;
        }
        statuses.push(WanStatus {
            id: target.interface.clone(),
            description: String::new(),
            interface_name: String::new(),
            state: "missing".to_string(),
            link: "down".to_string(),
            connected: "no".to_string(),
            address: None,
            mask: None,
            class: None,
            class_reason: None,
            uptime_secs: None,
            watched: true,
            enabled: target.enabled,
            hunting: false,
            phase: "idle".to_string(),
            restart_count: 0,
            last_restart: None,
            next_action_in_secs: None,
            last_error: Some("interface not found in RCI".to_string()),
            recovery_pending: state.is_pending_down(&target.interface),
        });
    }
    statuses.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(statuses)
}

fn wan_status_from_interface(
    interface: &crate::rci::RouterInterface,
    target: Option<&PppoeTargetSettings>,
    classifier: &Classifier,
    state: &StateFile,
) -> WanStatus {
    let classification = interface.address.map(|address| match target {
        Some(target) => target
            .classifier()
            .map(|target_classifier| target_classifier.classify(address))
            .unwrap_or_else(|_| classifier.classify(address)),
        None => classifier.classify(address),
    });
    WanStatus {
        id: interface.id.clone(),
        description: interface.description.clone(),
        interface_name: interface.interface_name.clone(),
        state: interface.state.clone(),
        link: interface.link.clone(),
        connected: interface.connected.clone(),
        address: interface.address,
        mask: interface.mask,
        class: classification.as_ref().map(|value| value.class),
        class_reason: classification.map(|value| value.reason),
        uptime_secs: interface.uptime,
        watched: target.is_some(),
        enabled: target.map(|target| target.enabled).unwrap_or(false),
        hunting: false,
        phase: "idle".to_string(),
        restart_count: 0,
        last_restart: None,
        next_action_in_secs: None,
        last_error: None,
        recovery_pending: state.is_pending_down(&interface.id),
    }
}

fn print_wan_table(statuses: &[WanStatus], style: Style) {
    let rows: Vec<Vec<String>> = statuses
        .iter()
        .enumerate()
        .map(|(index, interface)| {
            let watch = if interface.watched {
                if interface.enabled {
                    style.green("on").to_string()
                } else {
                    style.yellow("off").to_string()
                }
            } else {
                style.dim("-").to_string()
            };
            let mut phase = interface.phase.clone();
            if interface.hunting || phase != "idle" {
                phase = style.yellow(phase).to_string();
            }
            if let Some(error) = &interface.last_error {
                phase = format!("{}: {}", phase, truncate(error, 28));
            }
            vec![
                (index + 1).to_string(),
                interface.id.clone(),
                truncate(&interface.description, 22),
                interface
                    .address
                    .map(|address| address.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                style.class(interface.class).to_string(),
                format!("{}/{}", interface.state, interface.connected),
                watch,
                phase,
                interface.restart_count.to_string(),
            ]
        })
        .collect();
    print!(
        "{}",
        style.table(
            &["#", "ID", "Provider", "IPv4", "Class", "State", "Watch", "Phase", "Restarts"],
            &rows,
            &[true, false, false, false, false, false, false, false, true]
        )
    );
}

fn truncate(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    if width <= 1 {
        return "~".to_string();
    }
    let mut result: String = text.chars().take(width - 1).collect();
    result.push('~');
    result
}

fn run_pppoe(
    path: &Path,
    command: Option<PppoeCommand>,
    style: Style,
    json: bool,
) -> Result<(), String> {
    let config = Config::load(path)?;
    match command.unwrap_or(PppoeCommand::List) {
        PppoeCommand::List => pppoe_list(&config, style, json, false),
        PppoeCommand::Scan => pppoe_list(&config, style, json, true),
        PppoeCommand::Show { interface } => {
            let statuses = pppoe_statuses(&config, false)?;
            let selected = find_wan(&statuses, &interface)
                .ok_or_else(|| format!("interface {interface:?} not found"))?;
            print_wan_details(selected, &config, style);
            Ok(())
        }
        PppoeCommand::Restart { interface, force } => {
            pppoe_restart(&config, &interface, force, false, false, style)
        }
        PppoeCommand::Test {
            interface,
            dry_run,
            force,
        } => pppoe_restart(&config, &interface, force, true, dry_run, style),
        PppoeCommand::Watch { interface } => {
            let mut doc = ConfigDoc::load(path)?;
            doc.watch_target(&interface)?;
            save_doc_and_reload(path, &mut doc, &config, style)
        }
        PppoeCommand::Unwatch { interface } => {
            let mut doc = ConfigDoc::load(path)?;
            doc.set_target_enabled(&interface, false)?;
            save_doc_and_reload(path, &mut doc, &config, style)
        }
        PppoeCommand::Remove { interface, yes } => {
            if !yes
                && !ask::confirm(
                    &format!("remove PPPoE target {interface} and its rules?"),
                    false,
                )?
            {
                println!("not removed");
                return Ok(());
            }
            let mut doc = ConfigDoc::load(path)?;
            doc.remove_target(&interface)?;
            save_doc_and_reload(path, &mut doc, &config, style)
        }
        PppoeCommand::Rules { interface, edit } => {
            pppoe_rules(path, &config, interface, edit, style)
        }
    }
}

fn pppoe_statuses(config: &Config, force_scan: bool) -> Result<Vec<WanStatus>, String> {
    if let Some(client) = control_client(config) {
        let request = Request::new("pppoe-scan");
        let response = client.request(&request)?;
        if !response.ok {
            return Err(response
                .error
                .unwrap_or_else(|| "the daemon could not scan PPPoE".to_string()));
        }
        if let Some(data) = response.data {
            let value = data
                .get("interfaces")
                .cloned()
                .unwrap_or(Value::Array(Vec::new()));
            let mut statuses: Vec<WanStatus> = serde_json::from_value(value)
                .map_err(|err| format!("invalid daemon PPPoE list: {err}"))?;
            statuses.sort_by(|a, b| a.id.cmp(&b.id));
            return Ok(statuses);
        }
    }
    let _ = force_scan;
    direct_wan_statuses(config)
}

fn pppoe_list(config: &Config, style: Style, json: bool, _scan: bool) -> Result<(), String> {
    let statuses = pppoe_statuses(config, _scan)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "interfaces": statuses }))
                .map_err(|err| err.to_string())?
        );
        return Ok(());
    }
    if statuses.is_empty() {
        println!(
            "{}",
            style.yellow("no PPPoE interfaces reported by RCI; is this a Keenetic router?")
        );
        return Ok(());
    }
    println!("{}", style.heading("PPPoE tunnels (Keenetic)"));
    print_wan_table(&statuses, style);
    println!("\n{}", style.heading("actions"));
    println!(
        "  {}  one down/up cycle (daemon required)",
        style.bold("wolguard pppoe restart <id> --force")
    );
    println!(
        "  {}  one cycle with a live result",
        style.bold("wolguard pppoe test <id>")
    );
    println!(
        "  {}  add or enable a watched target",
        style.bold("wolguard pppoe watch <id>")
    );
    println!(
        "  {}  show rules; add --edit to change them",
        style.bold("wolguard pppoe rules <id>")
    );
    println!(
        "{} run `wolguard tui` for arrow-key selection",
        style.dim("tip:")
    );
    Ok(())
}

fn find_wan<'a>(statuses: &'a [WanStatus], selector: &str) -> Option<&'a WanStatus> {
    statuses
        .iter()
        .find(|status| status.id.eq_ignore_ascii_case(selector))
}

fn print_wan_details(status: &WanStatus, config: &Config, style: Style) {
    println!("{}", style.heading(&format!("PPPoE {}", status.id)));
    println!(
        "{} {}",
        style.dim("provider:"),
        if status.description.is_empty() {
            "-"
        } else {
            &status.description
        }
    );
    println!(
        "{} {}",
        style.dim("kernel:"),
        if status.interface_name.is_empty() {
            "-"
        } else {
            &status.interface_name
        }
    );
    println!(
        "{} {} ({})",
        style.dim("IPv4:"),
        status
            .address
            .map(|address| address.to_string())
            .unwrap_or_else(|| "-".to_string()),
        style.class(status.class)
    );
    if let Some(reason) = &status.class_reason {
        println!("{} {}", style.dim("class:"), reason);
    }
    println!(
        "{} {}/{} (connected {})",
        style.dim("state:"),
        status.state,
        status.link,
        status.connected
    );
    println!(
        "{} {}",
        style.dim("uptime:"),
        status
            .uptime_secs
            .map(format_seconds)
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "{} {}",
        style.dim("watched:"),
        if status.watched {
            if status.enabled {
                style.green("yes")
            } else {
                style.yellow("yes (disabled)")
            }
        } else {
            style.dim("no").to_string()
        }
    );
    println!("{} {}", style.dim("phase:"), status.phase);
    println!("{} {}", style.dim("restarts:"), status.restart_count);
    if let Some(restart) = status.last_restart {
        println!(
            "{} {}",
            style.dim("last restart:"),
            format_age(SystemTime::now(), Some(restart))
        );
    }
    if status.recovery_pending {
        println!(
            "{} {}",
            style.dim("recovery:"),
            style.yellow("pending (wolguard took this interface down)")
        );
    }
    if let Some(error) = &status.last_error {
        println!("{} {}", style.dim("last error:"), style.red(error));
    }
    println!(
        "{} wolguard pppoe restart {} --force",
        style.dim("restart via:"),
        status.id
    );
    let _ = config;
}

fn pppoe_restart(
    config: &Config,
    interface: &str,
    force: bool,
    test: bool,
    dry_run: bool,
    style: Style,
) -> Result<(), String> {
    let Some(client) = control_client(config) else {
        return Err(
            "PPPoE restarts must go through the daemon; start the service first: \
             `wolguard service start`. A one-shot restart can drop the SSH session and \
             leave the WAN down."
                .to_string(),
        );
    };
    let response = client.request(&Request::with_args(
        "pppoe-restart",
        json!({
            "interface": interface,
            "force": force,
            "test": test,
            "dry_run": dry_run,
        }),
    ))?;
    if !response.ok {
        return Err(response
            .error
            .unwrap_or_else(|| "the daemon refused the restart".to_string()));
    }
    if let Some(message) = response.data.as_ref().and_then(|data| data.get("message")) {
        println!("{}", message.as_str().unwrap_or("restart accepted"));
    }
    if test && !dry_run {
        println!(
            "{}",
            style.dim("watching the test cycle (Ctrl-C stops watching, not the daemon)")
        );
        let deadline = Instant::now() + Duration::from_secs(180);
        let mut last_phase = String::new();
        loop {
            thread::sleep(Duration::from_millis(800));
            let statuses = match pppoe_statuses(config, false) {
                Ok(statuses) => statuses,
                Err(err) => {
                    eprintln!("{} {err}", style.yellow("wolguard:"));
                    continue;
                }
            };
            let Some(status) = find_wan(&statuses, interface) else {
                continue;
            };
            let phase = format!(
                "{} {} {}",
                status.phase,
                status
                    .address
                    .map(|address| address.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                status.class.map(IpClass::label).unwrap_or("unknown")
            );
            if phase != last_phase {
                println!("{} {}: {}", style.dim("test:"), interface, phase);
                last_phase = phase;
            }
            if !status.hunting
                && matches!(
                    status.phase.as_str(),
                    "idle" | "waiting-retry" | "recovering"
                )
            {
                println!(
                    "{} {}",
                    style.dim("result:"),
                    status
                        .address
                        .map(|address| format!(
                            "{address} ({})",
                            status.class.map(IpClass::label).unwrap_or("unknown")
                        ))
                        .unwrap_or_else(|| "no usable address".to_string())
                );
                break;
            }
            if Instant::now() >= deadline {
                println!("{}", style.yellow("test: timed out waiting for the cycle"));
                break;
            }
        }
    }
    Ok(())
}

fn pppoe_rules(
    path: &Path,
    config: &Config,
    interface: Option<String>,
    edit: bool,
    style: Style,
) -> Result<(), String> {
    let targets = config.pppoe_targets()?;
    if targets.is_empty() {
        return Err("no PPPoE targets configured; use `wolguard pppoe watch <id>`".to_string());
    }
    let interface = match interface {
        Some(interface) => interface,
        None => {
            if !ask::stdin_is_tty() {
                for target in &targets {
                    println!("{}", target.interface);
                }
                return Err("choose an interface: `wolguard pppoe rules <id>`".to_string());
            }
            let options: Vec<String> = targets
                .iter()
                .map(|target| format!("{} ({})", target.interface, target.trigger.label()))
                .collect();
            let index = ask::choose("interface", &options, 0)?;
            targets[index].interface.clone()
        }
    };
    let target = targets
        .iter()
        .find(|target| target.interface.eq_ignore_ascii_case(&interface))
        .ok_or_else(|| format!("PPPoE target {interface:?} is not configured"))?;
    if !edit {
        println!(
            "{}",
            style.heading(&format!("PPPoE target {}", target.interface))
        );
        println!(
            "{} {}",
            style.dim("enabled:"),
            if target.enabled { "yes" } else { "no" }
        );
        println!("{} {}", style.dim("trigger:"), target.trigger.label());
        println!("{} {} s", style.dim("poll:"), target.poll.as_secs());
        println!("{} {} s", style.dim("down:"), target.down.as_secs());
        println!("{} {} s", style.dim("settle:"), target.settle.as_secs());
        println!("{} {} s", style.dim("retry:"), target.retry.as_secs());
        println!(
            "{} {}/hour",
            style.dim("max restarts:"),
            target.max_restarts_per_hour
        );
        println!(
            "{} {}",
            style.dim("white CIDRs:"),
            if target.white_cidrs.is_empty() {
                "(built-in public IPv4)".to_string()
            } else {
                target.white_cidrs.join(", ")
            }
        );
        println!(
            "{} {}",
            style.dim("grey CIDRs:"),
            if target.grey_cidrs.is_empty() {
                "(built-in special-use IPv4)".to_string()
            } else {
                target.grey_cidrs.join(", ")
            }
        );
        println!(
            "\n{} `wolguard pppoe rules {} --edit` to change",
            style.dim("tip:"),
            target.interface
        );
        return Ok(());
    }

    ask::require_tty()?;
    let trigger = loop {
        let value = ask::line("trigger (on-change/always)", Some(target.trigger.label()))?;
        match value.trim() {
            "on-change" => break Trigger::OnChange,
            "always" => break Trigger::Always,
            _ => eprintln!("choose on-change or always"),
        }
    };
    let poll = parse_u64_prompt("poll seconds", target.poll.as_secs())?;
    let down = parse_u64_prompt("down seconds", target.down.as_secs())?;
    let settle = parse_u64_prompt("settle seconds", target.settle.as_secs())?;
    let retry = parse_u64_prompt("retry seconds", target.retry.as_secs())?;
    let max =
        parse_u64_prompt("max restarts per hour", target.max_restarts_per_hour as u64)? as u32;
    let white = ask::parse_list(&ask::line(
        "white CIDRs (comma separated, empty = built-in public)",
        Some(&target.white_cidrs.join(",")),
    )?);
    let grey = ask::parse_list(&ask::line(
        "grey CIDRs (comma separated, empty = built-in special-use)",
        Some(&target.grey_cidrs.join(",")),
    )?);
    let mut doc = ConfigDoc::load(path)?;
    doc.set_target_rules(&TargetEdit {
        interface: target.interface.clone(),
        enabled: None,
        trigger: Some(trigger),
        poll_secs: Some(poll),
        down_secs: Some(down),
        settle_secs: Some(settle),
        retry_secs: Some(retry),
        max_restarts_per_hour: Some(max),
        white_cidrs: Some(white),
        grey_cidrs: Some(grey),
    })?;
    save_doc_and_reload(path, &mut doc, config, style)
}

fn parse_u64_prompt(label: &str, default: u64) -> Result<u64, String> {
    loop {
        let value = ask::line(label, Some(&default.to_string()))?;
        match value.trim().parse::<u64>() {
            Ok(value) if value > 0 => return Ok(value),
            _ => eprintln!("enter a positive integer"),
        }
    }
}

fn run_hosts(
    path: &Path,
    command: Option<HostsCommand>,
    style: Style,
    json: bool,
) -> Result<(), String> {
    let config = Config::load(path)?;
    match command.unwrap_or(HostsCommand::List) {
        HostsCommand::List => run_hosts_list(path, &config, style, json),
        HostsCommand::Add {
            name,
            ip,
            mac,
            interval,
            wake_retry,
            failures,
            max_wakes,
            disabled,
        } => {
            let edit = build_host_edit(
                path, None, name, ip, mac, interval, wake_retry, failures, max_wakes, disabled,
            )?;
            let mut doc = ConfigDoc::load(path)?;
            doc.upsert_host(&edit)?;
            save_doc_and_reload(path, &mut doc, &config, style)
        }
        HostsCommand::Edit {
            name,
            new_name,
            ip,
            mac,
            interval,
            wake_retry,
            failures,
            max_wakes,
            disabled,
        } => {
            let edit = build_host_edit(
                path,
                Some((&name, new_name)),
                None,
                ip,
                mac,
                interval,
                wake_retry,
                failures,
                max_wakes,
                disabled,
            )?;
            let mut doc = ConfigDoc::load(path)?;
            doc.upsert_host(&edit)?;
            save_doc_and_reload(path, &mut doc, &config, style)
        }
        HostsCommand::Remove { name, yes } => {
            if !yes && !ask::confirm(&format!("remove host {name}?"), false)? {
                println!("not removed");
                return Ok(());
            }
            let mut doc = ConfigDoc::load(path)?;
            doc.remove_host(&name)?;
            save_doc_and_reload(path, &mut doc, &config, style)
        }
        HostsCommand::Enable { name } => {
            let mut doc = ConfigDoc::load(path)?;
            doc.set_host_enabled(&name, true)?;
            save_doc_and_reload(path, &mut doc, &config, style)
        }
        HostsCommand::Disable { name } => {
            let mut doc = ConfigDoc::load(path)?;
            doc.set_host_enabled(&name, false)?;
            save_doc_and_reload(path, &mut doc, &config, style)
        }
        HostsCommand::Wake { name } => wake_host(&config, &name),
        HostsCommand::Leases => run_hosts_leases(&config, style, json),
    }
}

fn fetch_leases(config: &Config) -> Result<Vec<DhcpBinding>, String> {
    let mut client = RciClient::from_url(
        &config.rci.url,
        &config.rci.token,
        Duration::from_millis(config.rci.timeout_ms),
    )?;
    client.dhcp_bindings().map_err(|err| err.to_string())
}

fn lease_label(lease: &DhcpBinding) -> String {
    let name = if !lease.name.is_empty() {
        lease.name.as_str()
    } else if !lease.hostname.is_empty() {
        lease.hostname.as_str()
    } else {
        "unknown"
    };
    format!("{name}  {}  {}", lease.ip, lease.mac)
}

fn run_hosts_leases(config: &Config, style: Style, json: bool) -> Result<(), String> {
    let leases = fetch_leases(config)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "leases": leases }))
                .map_err(|err| err.to_string())?
        );
        return Ok(());
    }
    if leases.is_empty() {
        println!("{}", style.yellow("RCI reports no DHCP bindings"));
        return Ok(());
    }
    let rows: Vec<Vec<String>> = leases
        .iter()
        .map(|lease| {
            vec![
                if lease.name.is_empty() {
                    lease.hostname.clone()
                } else {
                    lease.name.clone()
                },
                lease.ip.clone(),
                lease.mac.clone(),
                lease.interface.clone(),
            ]
        })
        .collect();
    println!(
        "{}",
        style.heading(&format!("DHCP bindings ({})", leases.len()))
    );
    print!(
        "{}",
        style.table(
            &["Name", "IP", "MAC", "Interface"],
            &rows,
            &[false, false, false, false]
        )
    );
    println!(
        "{} use `wolguard hosts add --name NAME --ip IP --mac MAC`",
        style.dim("tip:")
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_host_edit(
    path: &Path,
    editing: Option<(&str, Option<String>)>,
    name: Option<String>,
    ip: Option<String>,
    mac: Option<String>,
    interval: Option<u64>,
    wake_retry: Option<u64>,
    failures: Option<u32>,
    max_wakes: Option<u32>,
    disabled: bool,
) -> Result<HostEdit, String> {
    let config = Config::load(path)?;
    let existing = editing.as_ref().and_then(|(original, _)| {
        config
            .hosts
            .iter()
            .find(|host| host.name == *original)
            .cloned()
    });
    if editing.is_some() && existing.is_none() {
        return Err(format!(
            "host {:?} is not configured",
            editing.as_ref().map(|(name, _)| *name).unwrap_or("")
        ));
    }

    let interactive = ask::stdin_is_tty()
        && name.is_none()
        && ip.is_none()
        && mac.is_none()
        && interval.is_none()
        && wake_retry.is_none()
        && failures.is_none()
        && max_wakes.is_none()
        && !disabled;
    if interactive {
        let mut lease_defaults = None;
        if editing.is_none() {
            if let Ok(leases) = fetch_leases(&config) {
                if !leases.is_empty()
                    && ask::confirm("choose a host from the router's DHCP leases?", true)?
                {
                    let options: Vec<String> = leases.iter().map(lease_label).collect();
                    let index = ask::choose("lease", &options, 0)?;
                    lease_defaults = Some(leases[index].clone());
                }
            }
        }
        let lease_name = lease_defaults.as_ref().map(|lease| {
            if !lease.name.is_empty() {
                lease.name.clone()
            } else if !lease.hostname.is_empty() {
                lease.hostname.clone()
            } else {
                String::new()
            }
        });
        let default_name = existing
            .as_ref()
            .map(|host| host.name.clone())
            .or_else(|| name.clone())
            .or(lease_name);
        let default_ip = existing
            .as_ref()
            .map(|host| host.ip.to_string())
            .or_else(|| ip.clone())
            .or_else(|| lease_defaults.as_ref().map(|lease| lease.ip.clone()));
        let default_mac = existing
            .as_ref()
            .map(|host| host.mac.clone())
            .or_else(|| mac.clone())
            .or_else(|| lease_defaults.as_ref().map(|lease| lease.mac.clone()));
        let name = ask::required("host name", default_name.as_deref())?;
        let ip = ask::required("IPv4 address", default_ip.as_deref())?;
        let mac = ask::required("MAC address", default_mac.as_deref())?;
        let interval = ask::parse_optional_u64(&ask::line(
            "check interval seconds ('-' = global default)",
            Some(
                &existing
                    .as_ref()
                    .and_then(|host| host.check_interval_secs)
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_string()),
            ),
        )?)?;
        let wake_retry = ask::parse_optional_u64(&ask::line(
            "wake retry seconds ('-' = global default)",
            Some(
                &existing
                    .as_ref()
                    .and_then(|host| host.wake_retry_secs)
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_string()),
            ),
        )?)?;
        let failures = ask::parse_optional_u64(&ask::line(
            "failures before wake ('-' = global default)",
            Some(
                &existing
                    .as_ref()
                    .and_then(|host| host.failures_before_wake)
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_string()),
            ),
        )?)?;
        let max_wakes = ask::parse_optional_u64(&ask::line(
            "max wakes per hour ('-' = global default)",
            Some(
                &existing
                    .as_ref()
                    .and_then(|host| host.max_wakes_per_hour)
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_string()),
            ),
        )?)?;
        let enabled = ask::confirm(
            "monitoring enabled?",
            existing.as_ref().map(|host| host.enabled).unwrap_or(true),
        )?;
        return Ok(HostEdit {
            original_name: editing.as_ref().map(|(original, _)| original.to_string()),
            name,
            ip,
            mac,
            enabled,
            check_interval_secs: interval,
            wake_retry_secs: wake_retry,
            failures_before_wake: failures.map(|value| value as u32),
            max_wakes_per_hour: max_wakes.map(|value| value as u32),
        });
    }

    let name = name
        .or_else(|| editing.as_ref().and_then(|(_, new_name)| new_name.clone()))
        .or_else(|| existing.as_ref().map(|host| host.name.clone()))
        .ok_or_else(|| "host name is required: --name NAME".to_string())?;
    let ip = ip
        .or_else(|| existing.as_ref().map(|host| host.ip.to_string()))
        .ok_or_else(|| "IPv4 address is required: --ip ADDRESS".to_string())?;
    let mac = mac
        .or_else(|| existing.as_ref().map(|host| host.mac.clone()))
        .ok_or_else(|| "MAC address is required: --mac MAC".to_string())?;
    ip.parse::<Ipv4Addr>()
        .map_err(|_| format!("invalid IPv4 address {ip:?}"))?;
    config::parse_mac(&mac)?;
    Ok(HostEdit {
        original_name: editing.as_ref().map(|(original, _)| original.to_string()),
        name,
        ip,
        mac,
        enabled: if disabled {
            false
        } else {
            existing.as_ref().map(|host| host.enabled).unwrap_or(true)
        },
        check_interval_secs: interval
            .or_else(|| existing.as_ref().and_then(|host| host.check_interval_secs)),
        wake_retry_secs: wake_retry
            .or_else(|| existing.as_ref().and_then(|host| host.wake_retry_secs)),
        failures_before_wake: failures
            .or_else(|| existing.as_ref().and_then(|host| host.failures_before_wake)),
        max_wakes_per_hour: max_wakes
            .or_else(|| existing.as_ref().and_then(|host| host.max_wakes_per_hour)),
    })
}

fn run_hosts_list(path: &Path, config: &Config, style: Style, json: bool) -> Result<(), String> {
    let snapshot = control_client(config)
        .and_then(|client| client.status().ok())
        .and_then(|data| snapshot_from_data(&data).ok());
    if json {
        let hosts: Vec<Value> = config
            .hosts
            .iter()
            .map(|host| {
                let live = snapshot.as_ref().and_then(|snapshot| {
                    snapshot
                        .hosts
                        .iter()
                        .find(|status| status.name == host.name)
                });
                json!({
                    "name": host.name,
                    "ip": host.ip.to_string(),
                    "mac": host.mac,
                    "enabled": host.enabled && config.wol_enabled,
                    "state": live.map(|status| status.state.label()).unwrap_or("unknown"),
                    "rtt_ms": live.and_then(|status| status.last_rtt).map(|rtt| rtt.as_millis() as u64),
                    "wakes": live.map(|status| status.wake_count).unwrap_or(0),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({ "config": path.display().to_string(), "hosts": hosts })
            )
            .map_err(|err| err.to_string())?
        );
        return Ok(());
    }
    if config.hosts.is_empty() {
        println!(
            "{}",
            style.yellow("no hosts configured; use `wolguard hosts add`")
        );
        return Ok(());
    }
    let rows: Vec<Vec<String>> = config
        .hosts
        .iter()
        .enumerate()
        .map(|(index, host)| {
            let live = snapshot.as_ref().and_then(|snapshot| {
                snapshot
                    .hosts
                    .iter()
                    .find(|status| status.name == host.name)
            });
            vec![
                (index + 1).to_string(),
                host.name.clone(),
                host.ip.to_string(),
                host.mac.clone(),
                if host.enabled && config.wol_enabled {
                    style.green("yes").to_string()
                } else {
                    style.dim("no").to_string()
                },
                live.map(|status| status.state.label().to_string())
                    .unwrap_or_else(|| "-".to_string()),
                live.and_then(|status| status.last_rtt)
                    .map(format_millis)
                    .unwrap_or_else(|| "-".to_string()),
                host.check_interval_secs
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| format!("{}*", config.check_interval_secs)),
            ]
        })
        .collect();
    println!(
        "{}",
        style.heading(&format!("hosts ({})", config.hosts.len()))
    );
    print!(
        "{}",
        style.table(
            &["#", "Name", "IP", "MAC", "Enabled", "State", "RTT", "Every"],
            &rows,
            &[true, false, false, false, false, false, true, true]
        )
    );
    Ok(())
}

fn save_doc_and_reload(
    path: &Path,
    doc: &mut ConfigDoc,
    config: &Config,
    style: Style,
) -> Result<(), String> {
    doc.save(false)?;
    println!("{} {}", style.green("saved"), path.display());
    let new_config = Config::load(path)?;
    if let Some(client) = control_client(config) {
        if let Ok(data) = client.status() {
            if let Some(daemon_path) = data.get("config_path").and_then(Value::as_str) {
                if Path::new(daemon_path) != path {
                    eprintln!(
                        "{} the daemon uses {}, not {}; the reload applies the daemon's file",
                        style.yellow("warning:"),
                        daemon_path,
                        path.display()
                    );
                }
            }
        }
        match client.request(&Request::new("reload")) {
            Ok(response) if response.ok => {
                if let Some(restart) = response
                    .data
                    .as_ref()
                    .and_then(|data| data.get("restart_required"))
                    .and_then(Value::as_array)
                {
                    let names: Vec<String> = restart
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToString::to_string)
                        .collect();
                    if !names.is_empty() {
                        println!(
                            "{} restart required for: {}; run `wolguard service restart`",
                            style.yellow("note:"),
                            names.join(", ")
                        );
                    }
                }
                println!("{}", style.green("daemon reloaded"));
            }
            Ok(response) => {
                eprintln!(
                    "{} {}",
                    style.red("reload failed:"),
                    response
                        .error
                        .unwrap_or_else(|| "unknown error".to_string())
                );
            }
            Err(err) => eprintln!("{} {err}", style.yellow("reload:")),
        }
    } else {
        println!(
            "{} service is not running; the new configuration applies on start",
            style.dim("note:")
        );
    }
    let _ = new_config;
    Ok(())
}

fn wake_host(config: &Config, selector: &str) -> Result<(), String> {
    if let Some(client) = control_client(config) {
        let response = client.request(&Request::with_args("wake", json!({ "host": selector })))?;
        if response.ok {
            println!(
                "{}",
                response
                    .data
                    .as_ref()
                    .and_then(|data| data.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("wake packet queued")
            );
            return Ok(());
        }
        return Err(response.error.unwrap_or_else(|| "wake failed".to_string()));
    }
    let hosts = config.host_settings()?;
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

fn run_wake(path: &Path, selector: &str) -> Result<(), String> {
    let (config, hosts) = load_config(path)?;
    if control_client(&config).is_some() {
        return wake_host(&config, selector);
    }
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

fn run_settings(
    path: &Path,
    command: Option<SettingsCommand>,
    style: Style,
    json: bool,
) -> Result<(), String> {
    match command.unwrap_or(SettingsCommand::Show) {
        SettingsCommand::Show => {
            let config = Config::load(path)?;
            print_resolved_config(&config, path, style, json)
        }
        SettingsCommand::Set { key, value } => settings_set(path, &key, &value, style),
        SettingsCommand::Edit => {
            ask::require_tty()?;
            let config = Config::load(path)?;
            let mut doc = ConfigDoc::load(path)?;
            let mut changed = false;
            let wol = ask::confirm("LAN host monitoring (wol_enabled)?", config.wol_enabled)?;
            if wol != config.wol_enabled {
                doc.set_wol_enabled(wol);
                changed = true;
            }
            let pppoe = ask::confirm("PPPoE watchdog enabled?", config.pppoe.enabled)?;
            if pppoe != config.pppoe.enabled {
                doc.set_pppoe_enabled(pppoe)?;
                changed = true;
            }
            let trigger = loop {
                let value = ask::line(
                    "PPPoE trigger (on-change/always)",
                    Some(config.pppoe.trigger.label()),
                )?;
                match value.as_str() {
                    "on-change" => break Trigger::OnChange,
                    "always" => break Trigger::Always,
                    _ => eprintln!("choose on-change or always"),
                }
            };
            if trigger != config.pppoe.trigger {
                let defaults = pppoe_defaults(&config, |defaults| {
                    defaults.trigger = trigger;
                });
                doc.set_pppoe_defaults(&defaults)?;
                changed = true;
            }
            let max = parse_u64_prompt(
                "PPPoE max restarts per hour",
                config.pppoe.max_restarts_per_hour as u64,
            )? as u32;
            if max != config.pppoe.max_restarts_per_hour {
                let defaults = pppoe_defaults(&config, |defaults| {
                    defaults.max_restarts_per_hour = max;
                });
                doc.set_pppoe_defaults(&defaults)?;
                changed = true;
            }
            if changed {
                save_doc_and_reload(path, &mut doc, &config, style)
            } else {
                println!("nothing changed");
                Ok(())
            }
        }
    }
}

fn settings_set(path: &Path, key: &str, raw: &str, style: Style) -> Result<(), String> {
    let config = Config::load(path)?;
    let mut doc = ConfigDoc::load(path)?;
    match key {
        "wol_enabled" => doc.set_wol_enabled(ask::parse_bool(raw)?),
        "check_interval_secs" => doc.set_top_int(key, parse_positive(raw)?),
        "ping_timeout_ms" => doc.set_top_int(key, parse_positive(raw)?),
        "failures_before_wake" => doc.set_top_int(key, parse_positive(raw)?),
        "wake_retry_secs" => doc.set_top_int(key, parse_positive(raw)?),
        "max_wakes_per_hour" => doc.set_top_int(key, parse_positive(raw)?),
        "wol_port" => doc.set_top_int(key, parse_positive(raw)?),
        "wake_on_start" => doc.set_top_bool(key, ask::parse_bool(raw)?),
        "verbose_probes" => doc.set_top_bool(key, ask::parse_bool(raw)?),
        "broadcast" => doc.set_top_string(key, raw),
        "log_file" => doc.set_top_string(key, raw),
        "pid_file" => doc.set_top_string(key, raw),
        "control_socket" => doc.set_top_string(key, raw),
        "init_script" => doc.set_top_string(key, raw),
        "state_file" => doc.set_top_string(key, raw),
        "rci.url" => {
            let mut edit = rci_edit(&config);
            edit.url = raw.to_string();
            doc.set_rci(&edit)?;
        }
        "rci.token" => {
            let mut edit = rci_edit(&config);
            edit.token = raw.to_string();
            doc.set_rci(&edit)?;
        }
        "rci.timeout_ms" => {
            let mut edit = rci_edit(&config);
            edit.timeout_ms = parse_positive(raw)?;
            doc.set_rci(&edit)?;
        }
        "rci.auto_token" => {
            let mut edit = rci_edit(&config);
            edit.auto_token = ask::parse_bool(raw)?;
            doc.set_rci(&edit)?;
        }
        "rci.ndmc_path" => {
            let mut edit = rci_edit(&config);
            edit.ndmc_path = raw.to_string();
            doc.set_rci(&edit)?;
        }
        "pppoe.enabled" => doc.set_pppoe_enabled(ask::parse_bool(raw)?)?,
        "pppoe.trigger" => {
            let trigger = parse_trigger(raw)?;
            let defaults = pppoe_defaults(&config, |defaults| defaults.trigger = trigger);
            doc.set_pppoe_defaults(&defaults)?;
        }
        "pppoe.poll_secs" => {
            let value = parse_positive(raw)?;
            let defaults = pppoe_defaults(&config, |defaults| defaults.poll_secs = value);
            doc.set_pppoe_defaults(&defaults)?;
        }
        "pppoe.down_secs" => {
            let value = parse_positive(raw)?;
            let defaults = pppoe_defaults(&config, |defaults| defaults.down_secs = value);
            doc.set_pppoe_defaults(&defaults)?;
        }
        "pppoe.settle_secs" => {
            let value = parse_positive(raw)?;
            let defaults = pppoe_defaults(&config, |defaults| defaults.settle_secs = value);
            doc.set_pppoe_defaults(&defaults)?;
        }
        "pppoe.retry_secs" => {
            let value = parse_positive(raw)?;
            let defaults = pppoe_defaults(&config, |defaults| defaults.retry_secs = value);
            doc.set_pppoe_defaults(&defaults)?;
        }
        "pppoe.max_restarts_per_hour" => {
            let value = parse_positive(raw)? as u32;
            let defaults =
                pppoe_defaults(&config, |defaults| defaults.max_restarts_per_hour = value);
            doc.set_pppoe_defaults(&defaults)?;
        }
        "pppoe.white_cidrs" => {
            let value = ask::parse_list(raw);
            let defaults = pppoe_defaults(&config, |defaults| defaults.white_cidrs = value);
            doc.set_pppoe_defaults(&defaults)?;
        }
        "pppoe.grey_cidrs" => {
            let value = ask::parse_list(raw);
            let defaults = pppoe_defaults(&config, |defaults| defaults.grey_cidrs = value);
            doc.set_pppoe_defaults(&defaults)?;
        }
        other => {
            return Err(format!(
                "unknown setting {other:?}; try `wolguard settings show` or edit it in the TUI"
            ))
        }
    }
    save_doc_and_reload(path, &mut doc, &config, style)
}

fn parse_positive(raw: &str) -> Result<u64, String> {
    let value: u64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("expected a positive integer, got {raw:?}"))?;
    if value == 0 {
        return Err("value must be greater than zero".to_string());
    }
    Ok(value)
}

fn parse_trigger(raw: &str) -> Result<Trigger, String> {
    match raw.trim() {
        "on-change" => Ok(Trigger::OnChange),
        "always" => Ok(Trigger::Always),
        other => Err(format!(
            "trigger must be on-change or always, got {other:?}"
        )),
    }
}

fn rci_edit(config: &Config) -> RciEdit {
    RciEdit {
        url: config.rci.url.clone(),
        token: config.rci.token.clone(),
        timeout_ms: config.rci.timeout_ms,
        auto_token: config.rci.auto_token,
        ndmc_path: config.rci.ndmc_path.clone(),
    }
}

fn pppoe_defaults(config: &Config, patch: impl FnOnce(&mut PppoeDefaults)) -> PppoeDefaults {
    let mut defaults = PppoeDefaults {
        trigger: config.pppoe.trigger,
        poll_secs: config.pppoe.poll_secs,
        down_secs: config.pppoe.down_secs,
        settle_secs: config.pppoe.settle_secs,
        retry_secs: config.pppoe.retry_secs,
        max_restarts_per_hour: config.pppoe.max_restarts_per_hour,
        white_cidrs: config.pppoe.white_cidrs.clone(),
        grey_cidrs: config.pppoe.grey_cidrs.clone(),
    };
    patch(&mut defaults);
    defaults
}

fn print_resolved_config(
    config: &Config,
    path: &Path,
    style: Style,
    json: bool,
) -> Result<(), String> {
    let targets = config.pppoe_targets()?;
    if json {
        let target_values: Vec<Value> = targets
            .iter()
            .map(|target| {
                json!({
                    "interface": target.interface,
                    "enabled": target.enabled,
                    "trigger": target.trigger.label(),
                    "poll_secs": target.poll.as_secs(),
                    "down_secs": target.down.as_secs(),
                    "settle_secs": target.settle.as_secs(),
                    "retry_secs": target.retry.as_secs(),
                    "max_restarts_per_hour": target.max_restarts_per_hour,
                    "white_cidrs": target.white_cidrs,
                    "grey_cidrs": target.grey_cidrs,
                })
            })
            .collect();
        let host_values: Vec<Value> = config
            .hosts
            .iter()
            .map(|host| {
                json!({
                    "name": host.name,
                    "ip": host.ip.to_string(),
                    "mac": host.mac,
                    "enabled": host.enabled,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "config_path": path.display().to_string(),
                "wol_enabled": config.wol_enabled,
                "check_interval_secs": config.check_interval_secs,
                "ping_timeout_ms": config.ping_timeout_ms,
                "failures_before_wake": config.failures_before_wake,
                "wake_retry_secs": config.wake_retry_secs,
                "max_wakes_per_hour": config.max_wakes_per_hour,
                "wake_on_start": config.wake_on_start,
                "broadcast": config.broadcast,
                "wol_port": config.wol_port,
                "log_file": config.log_file,
                "pid_file": config.pid_file,
                "control_socket": config.control_socket,
                "init_script": config.init_script,
                "state_file": config.state_file,
                "rci": {
                    "url": config.rci.url,
                    "token_set": !config.rci.token.is_empty(),
                    "timeout_ms": config.rci.timeout_ms,
                    "auto_token": config.rci.auto_token,
                },
                "pppoe": {
                    "enabled": config.pppoe.enabled,
                    "trigger": config.pppoe.trigger.label(),
                    "poll_secs": config.pppoe.poll_secs,
                    "down_secs": config.pppoe.down_secs,
                    "settle_secs": config.pppoe.settle_secs,
                    "retry_secs": config.pppoe.retry_secs,
                    "max_restarts_per_hour": config.pppoe.max_restarts_per_hour,
                    "white_cidrs": config.pppoe.white_cidrs,
                    "grey_cidrs": config.pppoe.grey_cidrs,
                },
                "targets": target_values,
                "hosts": host_values,
            }))
            .map_err(|err| err.to_string())?
        );
        return Ok(());
    }

    println!("{}", style.heading("resolved configuration"));
    println!("{} {}", style.dim("file:"), path.display());
    println!(
        "{} {}",
        style.dim("log:"),
        if config.logs_to_stdout() {
            "stdout".to_string()
        } else {
            config.log_file.clone()
        }
    );
    println!("{} {}", style.dim("pid file:"), config.pid_file);
    println!("{} {}", style.dim("control socket:"), config.control_socket);
    println!("{} {}", style.dim("state file:"), config.state_file);
    println!("{} {}", style.dim("init script:"), config.init_script);
    println!(
        "{} {}",
        style.dim("WOL monitoring:"),
        if config.wol_enabled {
            style.green("enabled")
        } else {
            style.yellow("disabled")
        }
    );
    println!(
        "{} every {} s, timeout {} ms, wake after {} failures, retry {} s, max {}/hour",
        style.dim("WOL policy:"),
        config.check_interval_secs,
        config.ping_timeout_ms,
        config.failures_before_wake,
        config.wake_retry_secs,
        config.max_wakes_per_hour
    );
    println!(
        "{} {} (udp {})",
        style.dim("wake target:"),
        config.broadcast.as_deref().unwrap_or("auto"),
        config.wol_port
    );
    println!(
        "{} {} (token {}), timeout {} ms, auto token {}",
        style.dim("RCI:"),
        config.rci.url,
        if config.rci.token.is_empty() {
            "not set"
        } else {
            "set"
        },
        config.rci.timeout_ms,
        config.rci.auto_token
    );
    println!(
        "{} {} (trigger {}, poll {} s, retry {} s, max {}/hour)",
        style.dim("PPPoE:"),
        if config.pppoe.enabled {
            style.green("enabled")
        } else {
            style.yellow("disabled")
        },
        config.pppoe.trigger.label(),
        config.pppoe.poll_secs,
        config.pppoe.retry_secs,
        config.pppoe.max_restarts_per_hour
    );
    if !targets.is_empty() {
        let rows: Vec<Vec<String>> = targets
            .iter()
            .map(|target| {
                vec![
                    target.interface.clone(),
                    if target.enabled {
                        style.green("yes").to_string()
                    } else {
                        style.yellow("no").to_string()
                    },
                    target.trigger.label().to_string(),
                    format!("{} s", target.poll.as_secs()),
                    format!("{} s", target.retry.as_secs()),
                    target.max_restarts_per_hour.to_string(),
                ]
            })
            .collect();
        println!(
            "\n{}",
            style.heading(&format!("PPPoE targets ({})", targets.len()))
        );
        print!(
            "{}",
            style.table(
                &[
                    "Interface",
                    "Enabled",
                    "Trigger",
                    "Poll",
                    "Retry",
                    "Max/hour"
                ],
                &rows,
                &[false, false, false, true, true, true]
            )
        );
    }
    if !config.hosts.is_empty() {
        println!(
            "\n{}",
            style.heading(&format!("LAN hosts ({})", config.hosts.len()))
        );
        let rows: Vec<Vec<String>> = config
            .hosts
            .iter()
            .enumerate()
            .map(|(index, host)| {
                vec![
                    (index + 1).to_string(),
                    host.name.clone(),
                    host.ip.to_string(),
                    host.mac.clone(),
                    if host.enabled && config.wol_enabled {
                        "yes".to_string()
                    } else {
                        "no".to_string()
                    },
                ]
            })
            .collect();
        print!(
            "{}",
            style.table(
                &["#", "Name", "IP", "MAC", "Enabled"],
                &rows,
                &[true, false, false, false, false]
            )
        );
    }
    println!("\n{}", style.green("configuration OK"));
    Ok(())
}

fn run_check_config(path: &Path, style: Style) -> Result<(), String> {
    let config = Config::load(path)?;
    print_resolved_config(&config, path, style, false)
}

fn run_config(
    path: &Path,
    command: Option<ConfigCommand>,
    style: Style,
    json: bool,
) -> Result<(), String> {
    match command.unwrap_or(ConfigCommand::Show) {
        ConfigCommand::Show => {
            let text = std::fs::read_to_string(path)
                .map_err(|err| format!("cannot read config {}: {err}", path.display()))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "path": path.display().to_string(),
                        "toml": text,
                    }))
                    .map_err(|err| err.to_string())?
                );
            } else {
                print!("{text}");
            }
            Ok(())
        }
        ConfigCommand::Validate => {
            let config = Config::load(path)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "path": path.display().to_string(),
                        "hosts": config.hosts.len(),
                        "pppoe_targets": config.pppoe.targets.len(),
                    }))
                    .map_err(|err| err.to_string())?
                );
            } else {
                println!("{}", style.green("configuration OK"));
            }
            Ok(())
        }
        ConfigCommand::Path => {
            println!("{}", path.display());
            Ok(())
        }
    }
}

fn run_logs(path: &Path, lines: usize, follow: bool, style: Style) -> Result<(), String> {
    let config = Config::load(path)?;
    if let Some(client) = control_client(&config) {
        let limit = lines.max(1);
        let response = client.request(&Request::with_args(
            "logs",
            json!({ "after": 0, "limit": limit }),
        ))?;
        if !response.ok {
            return Err(response
                .error
                .unwrap_or_else(|| "cannot read daemon logs".to_string()));
        }
        let data = response.data.unwrap_or(Value::Null);
        let mut last_seq = 0;
        if let Some(entries) = data.get("lines").and_then(Value::as_array) {
            for entry in entries {
                if let Ok(line) = serde_json::from_value::<crate::model::LogLine>(entry.clone()) {
                    print_log_line(&line);
                    last_seq = last_seq.max(line.seq);
                }
            }
        }
        if follow {
            loop {
                thread::sleep(Duration::from_millis(500));
                let response = client.request(&Request::with_args(
                    "logs",
                    json!({ "after": last_seq, "limit": 200 }),
                ))?;
                if !response.ok {
                    return Err(response
                        .error
                        .unwrap_or_else(|| "cannot read daemon logs".to_string()));
                }
                if let Some(entries) = response
                    .data
                    .as_ref()
                    .and_then(|data| data.get("lines"))
                    .and_then(Value::as_array)
                {
                    for entry in entries {
                        if let Ok(line) =
                            serde_json::from_value::<crate::model::LogLine>(entry.clone())
                        {
                            print_log_line(&line);
                            last_seq = last_seq.max(line.seq);
                        }
                    }
                }
            }
        }
        return Ok(());
    }

    if config.logs_to_stdout() {
        return Err(
            "the daemon logs to stdout and is not running; start the service and use \
             `wolguard logs`"
                .to_string(),
        );
    }
    let text = std::fs::read_to_string(&config.log_file)
        .map_err(|err| format!("cannot read log {}: {err}", config.log_file))?;
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines.max(1));
    for line in &all[start..] {
        println!("{line}");
    }
    if follow {
        let mut offset = std::fs::metadata(&config.log_file)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        loop {
            thread::sleep(Duration::from_millis(500));
            let Ok(metadata) = std::fs::metadata(&config.log_file) else {
                continue;
            };
            if metadata.len() < offset {
                offset = 0;
            }
            if metadata.len() == offset {
                continue;
            }
            if let Ok(mut file) = File::open(&config.log_file) {
                use std::io::{Seek, SeekFrom};
                if file.seek(SeekFrom::Start(offset)).is_ok() {
                    let mut chunk = String::new();
                    if std::io::Read::read_to_string(&mut file, &mut chunk).is_ok() {
                        print!("{chunk}");
                        offset = metadata.len();
                    }
                }
            }
        }
    }
    let _ = style;
    Ok(())
}

fn print_log_line(line: &crate::model::LogLine) {
    println!(
        "{} [{}] {}",
        format_hms(line.at),
        line.level.label(),
        line.text
    );
}

fn run_service(path: &Path, action: ServiceAction, style: Style) -> Result<(), String> {
    let config = Config::load(path)?;
    let script = Path::new(&config.init_script);
    if !script.exists() {
        return Err(format!(
            "init script {} is missing; run `{} {}` manually",
            config.init_script,
            config.init_script,
            action.as_str()
        ));
    }
    let status = ProcessCommand::new(script)
        .arg(action.as_str())
        .status()
        .map_err(|err| format!("cannot run {}: {err}", config.init_script))?;
    if action == ServiceAction::Status {
        if let Some(client) = control_client(&config) {
            let _ = client.ping();
            println!(
                "{} control socket {} is responding",
                style.dim("note:"),
                config.control_socket
            );
        }
    }
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{} {} failed with {}",
            config.init_script,
            action.as_str(),
            status
        ))
    }
}

fn run_doctor(path: &Path, style: Style) -> Result<(), String> {
    let mut problems = 0u32;
    println!("{}", style.heading("wolguard doctor"));
    println!("{} {}", style.dim("config:"), path.display());

    let config = match Config::load(path) {
        Ok(config) => {
            println!("{}", style.green("ok   configuration parses and validates"));
            config
        }
        Err(err) => {
            println!("{} {err}", style.red("fail configuration:"));
            return Err("configuration is invalid; fix it first".to_string());
        }
    };

    match InstanceLock::probe(Path::new(&config.pid_file)) {
        Some(pid) => println!("{} daemon running (pid {pid})", style.green("ok  ")),
        None => println!("{} daemon is not running", style.yellow("warn")),
    }
    match control_client(&config) {
        Some(_) => println!(
            "{} control socket {} responds",
            style.green("ok  "),
            config.control_socket
        ),
        None => {
            problems += 1;
            println!(
                "{} control socket {} does not respond; start the service",
                style.red("fail"),
                config.control_socket
            );
        }
    }

    match RciClient::from_url(
        &config.rci.url,
        &config.rci.token,
        Duration::from_millis(config.rci.timeout_ms),
    ) {
        Ok(mut rci) => match rci.list_pppoe() {
            Ok(interfaces) => {
                println!(
                    "{} RCI {} reachable, {} PPPoE interface(s)",
                    style.green("ok  "),
                    config.rci.url,
                    interfaces.len()
                );
                let classifier =
                    Classifier::new(&config.pppoe.white_cidrs, &config.pppoe.grey_cidrs)
                        .unwrap_or_else(|_| Classifier::new(&[], &[]).expect("empty classifier"));
                for interface in &interfaces {
                    let class = interface
                        .address
                        .map(|address| classifier.classify(address).class);
                    println!(
                        "     {} {} {}",
                        interface.id,
                        interface
                            .address
                            .map(|address| address.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                        style.class(class)
                    );
                }
                if interfaces.is_empty() {
                    problems += 1;
                    println!("     {}", style.yellow("no PPPoE interfaces found"));
                }
            }
            Err(RciError::Unauthorized) => {
                problems += 1;
                println!("{} RCI requires a token", style.red("fail"));
                println!(
                    "     run `wolguard rci token` (uses ndmc) or set rci.token in the config"
                );
            }
            Err(err) => {
                problems += 1;
                println!("{} RCI: {err}", style.red("fail"));
            }
        },
        Err(err) => {
            problems += 1;
            println!("{} RCI configuration: {err}", style.red("fail"));
        }
    }

    match Pinger::new() {
        Ok(_) => println!("{} raw ICMP socket available", style.green("ok  ")),
        Err(err) => {
            problems += 1;
            println!("{} raw ICMP: {err} (run as root)", style.red("fail"));
        }
    }

    if config.logs_to_stdout() {
        println!("{} logging to stdout", style.dim("info"));
    } else {
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config.log_file)
        {
            Ok(_) => println!(
                "{} log file {} is writable",
                style.green("ok  "),
                config.log_file
            ),
            Err(err) => {
                problems += 1;
                println!("{} log file {}: {err}", style.red("fail"), config.log_file);
            }
        }
    }

    if Path::new(&config.init_script).exists() {
        println!(
            "{} init script {} exists",
            style.green("ok  "),
            config.init_script
        );
    } else {
        problems += 1;
        println!(
            "{} init script {} is missing",
            style.red("fail"),
            config.init_script
        );
    }

    match StateFile::load(Path::new(&config.state_file)) {
        Ok(state) if state.pending_down.is_empty() => {
            println!("{} no pending PPPoE recovery", style.green("ok  "));
        }
        Ok(state) => {
            problems += 1;
            println!(
                "{} pending recovery for: {}",
                style.yellow("warn"),
                state
                    .pending_down
                    .iter()
                    .map(|entry| entry.interface.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Err(err) => {
            problems += 1;
            println!("{} state file: {err}", style.red("fail"));
        }
    }

    if let Some(client) = control_client(&config) {
        if let Ok(data) = client.status() {
            if let Some(signals) = data
                .get("snapshot")
                .and_then(|snapshot| snapshot.get("session_signals"))
                .and_then(Value::as_array)
            {
                let signals: Vec<&str> = signals.iter().filter_map(Value::as_str).collect();
                println!(
                    "{} session-change signals: {}",
                    style.dim("info"),
                    signals.join(", ")
                );
            }
        }
    }

    if problems == 0 {
        println!("\n{}", style.green("doctor: no problems found"));
    } else {
        println!(
            "\n{}",
            style.yellow(format!("doctor: {problems} problem(s) found"))
        );
    }
    Ok(())
}

fn run_rci(path: &Path, command: RciCommand, style: Style) -> Result<(), String> {
    let config = Config::load(path)?;
    match command {
        RciCommand::Show => {
            println!("{} {}", style.dim("url:"), config.rci.url);
            println!(
                "{} {}",
                style.dim("token:"),
                if config.rci.token.is_empty() {
                    style.yellow("not set")
                } else {
                    style.green("set")
                }
            );
            println!("{} {} ms", style.dim("timeout:"), config.rci.timeout_ms);
            println!("{} {}", style.dim("auto token:"), config.rci.auto_token);
            println!("{} {}", style.dim("ndmc:"), config.rci.ndmc_path);
            if let Ok(mut client) = RciClient::from_url(
                &config.rci.url,
                &config.rci.token,
                Duration::from_millis(config.rci.timeout_ms),
            ) {
                match client.version() {
                    Ok(version) => println!("{} {version}", style.dim("NDM:")),
                    Err(err) => println!("{} {}", style.dim("NDM:"), style.yellow(err.to_string())),
                }
            }
            Ok(())
        }
        RciCommand::Token => {
            println!("generating an RCI access token via ndmc...");
            let token = rci::generate_token(&config.rci.ndmc_path)?;
            let mut doc = ConfigDoc::load(path)?;
            doc.set_rci(&RciEdit {
                url: config.rci.url.clone(),
                token,
                timeout_ms: config.rci.timeout_ms,
                auto_token: config.rci.auto_token,
                ndmc_path: config.rci.ndmc_path.clone(),
            })?;
            save_doc_and_reload(path, &mut doc, &config, style)?;
            println!(
                "{}",
                style.green("token generated and stored (value not printed)")
            );
            Ok(())
        }
    }
}

fn run_setup(path: &Path, style: Style) -> Result<(), String> {
    ask::require_tty()?;
    let loaded = Config::load(path);
    if let Err(err) = &loaded {
        if path.exists() {
            println!(
                "{} the existing configuration is invalid: {err}",
                style.yellow("warning:")
            );
        }
    }
    let config_exists = loaded.is_ok();
    let config = loaded.unwrap_or_else(|_| {
        Config::from_toml_str(
            "[[host]]\nname=\"placeholder\"\nip=\"192.168.1.2\"\nmac=\"AA:BB:CC:DD:EE:FF\"\n",
        )
        .expect("built-in setup defaults")
    });
    let mut doc = ConfigDoc::load(path)?;
    println!("{}", style.heading("wolguard setup"));
    println!("{} {}", style.dim("config:"), path.display());

    if ask::confirm(
        "add or update a LAN host?",
        !config_exists || config.hosts.is_empty(),
    )? {
        let name = ask::required("host name", None)?;
        let ip = ask::required("IPv4 address", None)?;
        let mac = ask::required("MAC address", None)?;
        let enabled = ask::confirm("monitoring enabled?", true)?;
        let edit = HostEdit {
            original_name: None,
            name,
            ip,
            mac,
            enabled,
            ..HostEdit::default()
        };
        doc.upsert_host(&edit)?;
        println!("{}", style.green("host added to the configuration"));
    }

    let interfaces = match direct_wan_statuses(&config) {
        Ok(interfaces) => interfaces,
        Err(err) => {
            println!("{} cannot scan PPPoE: {err}", style.yellow("warn"));
            Vec::new()
        }
    };
    if !interfaces.is_empty() {
        println!("\n{}", style.heading("PPPoE interfaces"));
        print_wan_table(&interfaces, style);
        if ask::confirm("watch a PPPoE interface for a white IPv4?", true)? {
            let options: Vec<String> = interfaces
                .iter()
                .map(|interface| {
                    format!(
                        "{} {} {}",
                        interface.id,
                        interface
                            .address
                            .map(|address| address.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                        interface.description
                    )
                })
                .collect();
            let index = ask::choose("interface", &options, 0)?;
            let interface = &interfaces[index];
            let trigger = loop {
                let value = ask::line(
                    "trigger (on-change/always)",
                    Some(config.pppoe.trigger.label()),
                )?;
                match value.as_str() {
                    "on-change" => break Trigger::OnChange,
                    "always" => break Trigger::Always,
                    _ => eprintln!("choose on-change or always"),
                }
            };
            let max = parse_u64_prompt(
                "max restarts per hour",
                config.pppoe.max_restarts_per_hour as u64,
            )? as u32;
            doc.watch_target(&interface.id)?;
            doc.set_target_rules(&TargetEdit {
                interface: interface.id.clone(),
                enabled: Some(true),
                trigger: Some(trigger),
                poll_secs: Some(config.pppoe.poll_secs),
                down_secs: Some(config.pppoe.down_secs),
                settle_secs: Some(config.pppoe.settle_secs),
                retry_secs: Some(config.pppoe.retry_secs),
                max_restarts_per_hour: Some(max),
                white_cidrs: None,
                grey_cidrs: None,
            })?;
            println!(
                "{}",
                style.green(format!("{} will be watched", interface.id))
            );
        }
    }

    doc.save(false)?;
    println!("{} {}", style.green("saved"), path.display());
    let new_config = Config::load(path)?;
    if let Some(client) = control_client(&new_config) {
        let response = client.request(&Request::new("reload"))?;
        if response.ok {
            println!("{}", style.green("daemon reloaded"));
        } else {
            return Err(response
                .error
                .unwrap_or_else(|| "daemon reload failed".to_string()));
        }
    } else if ask::confirm("start the wolguard service now?", true)? {
        let script = Path::new(&new_config.init_script);
        if script.exists() {
            let status = ProcessCommand::new(script)
                .arg("start")
                .status()
                .map_err(|err| format!("cannot run {}: {err}", new_config.init_script))?;
            if !status.success() {
                return Err(format!("{} start failed", new_config.init_script));
            }
        } else {
            println!(
                "{} init script {} not found; start `wolguard run` manually",
                style.yellow("warn"),
                new_config.init_script
            );
        }
    }
    println!(
        "\n{} run `wolguard` for the dashboard, `wolguard status` for a snapshot, \
         `wolguard doctor` to diagnose",
        style.dim("next:")
    );
    Ok(())
}

struct InstanceLock {
    path: PathBuf,
    _file: File,
}

enum LockError {
    AlreadyRunning(u32),
    Io(String),
}

impl InstanceLock {
    fn acquire(path: &Path) -> Result<InstanceLock, LockError> {
        // This catches a running daemon even on filesystems where flock is
        // unavailable (some USB volumes used by Entware).
        if let Some(pid) = Self::probe(path) {
            return Err(LockError::AlreadyRunning(pid));
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|err| {
                LockError::Io(format!("cannot create {}: {err}", parent.display()))
            })?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|err| {
                LockError::Io(format!("cannot open pid file {}: {err}", path.display()))
            })?;
        let rc = unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EWOULDBLOCK) => {
                    let pid = std::fs::read_to_string(path)
                        .ok()
                        .and_then(|text| text.trim().parse::<u32>().ok())
                        .unwrap_or(0);
                    return Err(LockError::AlreadyRunning(pid));
                }
                Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP) | Some(libc::ENOLCK) => {
                    // The filesystem cannot lock; the control-socket bind is
                    // the remaining duplicate guard.
                }
                _ => {
                    return Err(LockError::Io(format!(
                        "cannot lock pid file {}: {err}",
                        path.display()
                    )))
                }
            }
        }
        file.set_len(0)
            .map_err(|err| LockError::Io(format!("cannot truncate {}: {err}", path.display())))?;
        let mut writer = &file;
        writeln!(writer, "{}", std::process::id())
            .map_err(|err| LockError::Io(format!("cannot write {}: {err}", path.display())))?;
        let _ = writer.flush();
        Ok(InstanceLock {
            path: path.to_path_buf(),
            _file: file,
        })
    }

    fn probe(path: &Path) -> Option<u32> {
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
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn process_is_wolguard(pid: u32) -> bool {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
    if comm.trim() != "wolguard" {
        return false;
    }
    // The daemon is started as `wolguard ... run`; a CLI command such as
    // `wolguard logs --follow` must not be mistaken for it. A stale pid that
    // was reused by a CLI process is rare but easy to guard against.
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    cmdline
        .split(|byte| *byte == 0)
        .any(|argument| argument == b"run")
}

// Keep the old check-config output available for users who relied on it.
#[allow(dead_code)]
fn run_check_config_legacy(path: &Path) -> Result<(), String> {
    let (config, hosts) = load_config(path)?;
    println!("configuration: {}", path.display());
    println!("hosts: {}", hosts.len());
    for (index, host) in hosts.iter().enumerate() {
        let target = wol::resolve_wake_target(host.ip, config.broadcast.as_deref());
        println!(
            "  {}. {} ({}) mac {} - {}",
            index + 1,
            host.name,
            host.ip,
            format_mac(host.mac),
            target
        );
    }
    Ok(())
}

#[allow(dead_code)]
fn unused_dhcp_bindings(config: &Config) -> Result<Vec<DhcpBinding>, String> {
    let mut client = RciClient::from_url(
        &config.rci.url,
        &config.rci.token,
        Duration::from_millis(config.rci.timeout_ms),
    )?;
    client.dhcp_bindings().map_err(|err| err.to_string())
}

#[allow(dead_code)]
fn unused_phase(phase: Phase) -> &'static str {
    phase.label()
}

#[allow(dead_code)]
fn unused_ip_class(class: IpClass) -> &'static str {
    class.label()
}

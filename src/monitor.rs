//! Monitoring loop: ping hosts, update the shared snapshot, wake offline machines.

use std::collections::VecDeque;
use std::sync::mpsc::Receiver;
use std::sync::MutexGuard;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::config::{Config, HostSettings};
use crate::log::Logger;
use crate::model::{Command, HostState, HostStatus, SharedSnapshot, Snapshot};
use crate::ping::{PingOutcome, Pinger};
use crate::util::{format_mac, format_millis};
use crate::wol;

const TICK: Duration = Duration::from_millis(250);
const WAKE_HISTORY_WINDOW: Duration = Duration::from_secs(3600);

struct RuntimeHost {
    settings: HostSettings,
    next_check: Instant,
    wake_history: VecDeque<Instant>,
    ever_online: bool,
    last_error: Option<String>,
}

pub fn spawn(
    config: Config,
    logger: Logger,
    snapshot: SharedSnapshot,
    commands: Receiver<Command>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("monitor".to_string())
        .spawn(move || monitor_loop(config, logger, snapshot, commands))
        .expect("failed to spawn the monitor thread")
}

pub fn initial_snapshot(
    settings: &[HostSettings],
    config: &Config,
    config_path: String,
) -> Snapshot {
    let started_at = SystemTime::now();
    let hosts = settings
        .iter()
        .map(|host| HostStatus {
            name: host.name.clone(),
            ip: host.ip,
            mac: host.mac,
            enabled: host.enabled,
            state: if host.enabled {
                HostState::Unknown
            } else {
                HostState::Disabled
            },
            consecutive_failures: 0,
            last_rtt: None,
            last_ok: None,
            last_check: None,
            last_wake: None,
            wake_count: 0,
        })
        .collect();
    Snapshot {
        hosts,
        started_at,
        config_path,
        wol_port: config.wol_port,
        wake_target: summarize_wake_target(settings, config),
        last_update: started_at,
        wol_enabled: config.wol_enabled,
        wan: Vec::new(),
        session_signals: Vec::new(),
    }
}

fn summarize_wake_target(settings: &[HostSettings], config: &Config) -> String {
    let mut targets: Vec<String> = Vec::new();
    for host in settings {
        let target = wol::resolve_wake_target(host.ip, config.broadcast.as_deref()).to_string();
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    match targets.len() {
        0 => "no hosts".to_string(),
        1 => targets.remove(0),
        _ => format!("per host ({})", targets.join("; ")),
    }
}

fn monitor_loop(
    mut config: Config,
    logger: Logger,
    snapshot: SharedSnapshot,
    commands: Receiver<Command>,
) {
    let settings = match config.host_settings() {
        Ok(settings) => settings,
        Err(err) => {
            logger.error(format!("invalid configuration: {err}"));
            return;
        }
    };
    let mut timeout = config.ping_timeout();
    let mut pinger = match Pinger::new() {
        Ok(pinger) => Some(pinger),
        Err(err) => {
            logger.error(format!(
                "cannot open a raw ICMP socket: {err}; wolguard must run as root"
            ));
            None
        }
    };
    let mut hosts: Vec<RuntimeHost> = settings
        .into_iter()
        .map(|settings| RuntimeHost {
            settings,
            next_check: Instant::now(),
            wake_history: VecDeque::new(),
            ever_online: false,
            last_error: None,
        })
        .collect();

    logger.info(format!(
        "monitor started: {} host(s), ping timeout {}",
        hosts.len(),
        format_millis(timeout)
    ));
    for host in &hosts {
        let target = wol::resolve_wake_target(host.settings.ip, config.broadcast.as_deref());
        logger.info(format!(
            "wake target for {} ({}): {} (udp {})",
            host.settings.name, host.settings.ip, target, config.wol_port
        ));
        if target.iface.is_none() && !target.configured {
            logger.warn(format!(
                "could not derive the LAN broadcast address for {} ({}); using 255.255.255.255, \
                 set broadcast explicitly in the config if wake-ups fail",
                host.settings.name, host.settings.ip
            ));
        }
    }

    loop {
        if crate::stop_requested() {
            logger.info("stop requested, monitor is shutting down");
            return;
        }
        while let Ok(command) = commands.try_recv() {
            match command {
                Command::Shutdown => {
                    logger.info("monitor stopped");
                    return;
                }
                Command::WakeNow(name) => {
                    if let Some(index) = hosts.iter().position(|host| host.settings.name == name) {
                        if let Some(host) = hosts.get_mut(index) {
                            manual_wake(index, host, &config, &logger, &snapshot);
                        }
                    }
                }
                Command::ToggleEnabled(index) => {
                    if let Some(host) = hosts.get_mut(index) {
                        toggle_enabled(index, host, &logger, &snapshot);
                    }
                }
                Command::Reload {
                    config: new_config,
                    reply,
                } => {
                    let result = reload_hosts(
                        *new_config,
                        &mut config,
                        &mut hosts,
                        &mut timeout,
                        &logger,
                        &snapshot,
                    );
                    let _ = reply.send(result);
                }
            }
        }

        let now = Instant::now();
        for (index, host) in hosts.iter_mut().enumerate() {
            if !host.settings.enabled || now < host.next_check {
                continue;
            }
            host.next_check = now + host.settings.check_interval;
            check_host(
                index,
                host,
                &config,
                &logger,
                &snapshot,
                &mut pinger,
                timeout,
            );
        }
        thread::sleep(TICK);
    }
}

#[allow(clippy::too_many_arguments)]
fn reload_hosts(
    new_config: Config,
    config: &mut Config,
    hosts: &mut Vec<RuntimeHost>,
    timeout: &mut Duration,
    logger: &Logger,
    snapshot: &SharedSnapshot,
) -> Result<(), String> {
    new_config.validate()?;
    let settings = new_config.host_settings()?;

    let mut old_hosts = std::mem::take(hosts);
    let mut new_hosts = Vec::with_capacity(settings.len());
    for settings in settings {
        if let Some(index) = old_hosts.iter().position(|host| {
            host.settings.name == settings.name
                && host.settings.ip == settings.ip
                && host.settings.mac == settings.mac
        }) {
            let mut host = old_hosts.remove(index);
            host.settings = settings;
            new_hosts.push(host);
        } else {
            new_hosts.push(RuntimeHost {
                settings,
                next_check: Instant::now(),
                wake_history: VecDeque::new(),
                ever_online: false,
                last_error: None,
            });
        }
    }
    *hosts = new_hosts;
    *config = new_config;
    *timeout = config.ping_timeout();

    let now = SystemTime::now();
    let mut snapshot = lock(snapshot);
    let old_statuses = std::mem::take(&mut snapshot.hosts);
    let mut new_statuses = Vec::with_capacity(hosts.len());
    for host in hosts.iter() {
        if let Some(status) = old_statuses.iter().find(|status| {
            status.name == host.settings.name
                && status.ip == host.settings.ip
                && status.mac == host.settings.mac
        }) {
            new_statuses.push(status.clone());
        } else {
            new_statuses.push(HostStatus {
                name: host.settings.name.clone(),
                ip: host.settings.ip,
                mac: host.settings.mac,
                enabled: host.settings.enabled,
                state: if host.settings.enabled {
                    HostState::Unknown
                } else {
                    HostState::Disabled
                },
                consecutive_failures: 0,
                last_rtt: None,
                last_ok: None,
                last_check: None,
                last_wake: None,
                wake_count: 0,
            });
        }
    }
    for status in &mut new_statuses {
        if let Some(host) = hosts.iter().find(|host| host.settings.name == status.name) {
            status.enabled = host.settings.enabled;
            if !host.settings.enabled {
                status.state = HostState::Disabled;
            } else if matches!(status.state, HostState::Disabled) {
                status.state = HostState::Unknown;
            }
        }
    }
    snapshot.hosts = new_statuses;
    let settings: Vec<HostSettings> = hosts.iter().map(|host| host.settings.clone()).collect();
    snapshot.wake_target = summarize_wake_target(&settings, config);
    snapshot.wol_port = config.wol_port;
    snapshot.wol_enabled = config.wol_enabled;
    snapshot.last_update = now;
    logger.info(format!("configuration reloaded: {} host(s)", hosts.len()));
    Ok(())
}

fn check_host(
    index: usize,
    host: &mut RuntimeHost,
    config: &Config,
    logger: &Logger,
    snapshot: &SharedSnapshot,
    pinger: &mut Option<Pinger>,
    timeout: Duration,
) {
    let outcome = match pinger {
        Some(pinger) => pinger.ping(host.settings.ip, timeout),
        None => PingOutcome::Error(std::io::Error::other("raw ICMP socket unavailable")),
    };
    let now = SystemTime::now();
    match outcome {
        PingOutcome::Reply(rtt) => {
            host.ever_online = true;
            host.last_error = None;
            let became_online = {
                let mut snapshot = lock(snapshot);
                let status = &mut snapshot.hosts[index];
                let became_online = status.state != HostState::Online;
                status.state = HostState::Online;
                status.last_check = Some(now);
                status.last_ok = Some(now);
                status.last_rtt = Some(rtt);
                status.consecutive_failures = 0;
                snapshot.last_update = now;
                became_online
            };
            if became_online {
                logger.info(format!(
                    "{} ({}) is online, rtt {}",
                    host.settings.name,
                    host.settings.ip,
                    format_millis(rtt)
                ));
            }
        }
        PingOutcome::Timeout => {
            register_failure(index, host, config, logger, snapshot, now);
        }
        PingOutcome::Error(err) => {
            let text = err.to_string();
            if host.last_error.as_deref() != Some(text.as_str()) {
                logger.error(format!("ping {} failed: {text}", host.settings.ip));
                host.last_error = Some(text);
            }
            register_failure(index, host, config, logger, snapshot, now);
        }
    }
}

fn register_failure(
    index: usize,
    host: &mut RuntimeHost,
    config: &Config,
    logger: &Logger,
    snapshot: &SharedSnapshot,
    now: SystemTime,
) {
    let mut infos: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut wake_now = false;

    {
        let mut snapshot = lock(snapshot);
        let threshold = host.settings.failures_before_wake;
        let failures = {
            let status = &mut snapshot.hosts[index];
            status.last_check = Some(now);
            status.consecutive_failures = status.consecutive_failures.saturating_add(1);
            status.consecutive_failures
        };
        if failures == 1 || config.verbose_probes {
            infos.push(format!(
                "no reply from {} ({}) [{}/{}]",
                host.settings.name, host.settings.ip, failures, threshold
            ));
        }
        snapshot.last_update = now;

        if failures >= threshold {
            let first_offline = !matches!(
                snapshot.hosts[index].state,
                HostState::Offline | HostState::Waking
            );
            if first_offline {
                snapshot.hosts[index].state = HostState::Offline;
                warnings.push(format!(
                    "{} ({}) is offline ({} failed checks)",
                    host.settings.name, host.settings.ip, failures
                ));
            }

            let can_wake =
                config.wake_on_start || host.ever_online || snapshot.hosts[index].wake_count > 0;
            if !can_wake {
                if first_offline {
                    infos.push(format!(
                        "{} ({}): Wake-on-LAN suppressed (wake_on_start = false and the host was never seen online)",
                        host.settings.name, host.settings.ip
                    ));
                }
            } else {
                let cooldown_ok = snapshot.hosts[index]
                    .last_wake
                    .map(|last_wake| {
                        now.duration_since(last_wake)
                            .map(|elapsed| elapsed >= host.settings.wake_retry)
                            .unwrap_or(true)
                    })
                    .unwrap_or(true);
                if cooldown_ok {
                    let now_instant = Instant::now();
                    while let Some(oldest) = host.wake_history.front().copied() {
                        if now_instant.saturating_duration_since(oldest) >= WAKE_HISTORY_WINDOW {
                            host.wake_history.pop_front();
                        } else {
                            break;
                        }
                    }
                    if host.wake_history.len() as u32 >= host.settings.max_wakes_per_hour {
                        warnings.push(format!(
                            "{} ({}): wake rate limit reached ({}/hour), not sending Wake-on-LAN",
                            host.settings.name, host.settings.ip, host.settings.max_wakes_per_hour
                        ));
                    } else {
                        wake_now = true;
                    }
                }
            }
        }
    }

    for message in infos {
        logger.info(message);
    }
    for message in warnings {
        logger.warn(message);
    }
    if !wake_now {
        return;
    }

    let target = wol::resolve_wake_target(host.settings.ip, config.broadcast.as_deref());
    match wol::send_magic(
        host.settings.mac,
        target.address,
        target.iface.as_deref(),
        config.wol_port,
    ) {
        Ok(()) => {
            host.wake_history.push_back(Instant::now());
            let attempt = {
                let mut snapshot = lock(snapshot);
                let status = &mut snapshot.hosts[index];
                status.last_wake = Some(now);
                status.wake_count = status.wake_count.saturating_add(1);
                status.state = HostState::Waking;
                let attempt = status.wake_count;
                snapshot.last_update = now;
                attempt
            };
            logger.info(format!(
                "Wake-on-LAN sent to {} ({}) via {} (attempt {})",
                host.settings.name,
                format_mac(host.settings.mac),
                target,
                attempt
            ));
        }
        Err(err) => {
            logger.error(format!(
                "cannot send Wake-on-LAN to {}: {err}",
                host.settings.name
            ));
        }
    }
}

fn manual_wake(
    index: usize,
    host: &mut RuntimeHost,
    config: &Config,
    logger: &Logger,
    snapshot: &SharedSnapshot,
) {
    let target = wol::resolve_wake_target(host.settings.ip, config.broadcast.as_deref());
    match wol::send_magic(
        host.settings.mac,
        target.address,
        target.iface.as_deref(),
        config.wol_port,
    ) {
        Ok(()) => {
            host.wake_history.push_back(Instant::now());
            let now = SystemTime::now();
            let attempt = {
                let mut snapshot = lock(snapshot);
                let status = &mut snapshot.hosts[index];
                status.last_wake = Some(now);
                status.wake_count = status.wake_count.saturating_add(1);
                if matches!(status.state, HostState::Offline | HostState::Unknown) {
                    status.state = HostState::Waking;
                }
                let attempt = status.wake_count;
                snapshot.last_update = now;
                attempt
            };
            logger.info(format!(
                "manual Wake-on-LAN sent to {} ({}) via {} (attempt {})",
                host.settings.name,
                format_mac(host.settings.mac),
                target,
                attempt
            ));
        }
        Err(err) => logger.error(format!(
            "cannot send manual Wake-on-LAN to {}: {err}",
            host.settings.name
        )),
    }
}

fn toggle_enabled(
    index: usize,
    host: &mut RuntimeHost,
    logger: &Logger,
    snapshot: &SharedSnapshot,
) {
    host.settings.enabled = !host.settings.enabled;
    host.next_check = Instant::now();
    let now = SystemTime::now();
    {
        let mut snapshot = lock(snapshot);
        let status = &mut snapshot.hosts[index];
        status.enabled = host.settings.enabled;
        status.state = if host.settings.enabled {
            HostState::Unknown
        } else {
            HostState::Disabled
        };
        snapshot.last_update = now;
    }
    logger.info(format!(
        "monitoring for {} ({}) is {}",
        host.settings.name,
        host.settings.ip,
        if host.settings.enabled {
            "enabled"
        } else {
            "disabled"
        }
    ));
}

fn lock(snapshot: &SharedSnapshot) -> MutexGuard<'_, Snapshot> {
    snapshot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

//! PPPoE WAN watcher thread.
//!
//! Owns the RCI client, the pure PPPoE state machines, the persistent recovery
//! marker and the WAN snapshot. It runs independently of LAN host monitoring.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::MutexGuard;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{Config, Trigger};
use crate::configedit::{ConfigDoc, RciEdit};
use crate::ipclass::{Classifier, IpClass};
use crate::log::Logger;
use crate::model::{SharedSnapshot, Snapshot, WanStatus};
use crate::pppoe::{Action, Observation, Phase, Runtime, TargetSpec};
use crate::rci::{RciClient, RciError, RouterInterface};
use crate::statefile::{StateFile, StoredObservation};

const TICK: Duration = Duration::from_millis(250);
const HUNT_SCAN: Duration = Duration::from_secs(2);
const MAX_SCAN: Duration = Duration::from_secs(30);

pub enum WanCommand {
    /// Force a fresh RCI scan and return the current WAN list.
    Scan {
        reply: Sender<Result<Vec<WanStatus>, String>>,
    },
    /// Start one restart cycle (or a manual test) for an interface.
    Restart {
        interface: String,
        force: bool,
        oneshot: bool,
        dry_run: bool,
        reply: Sender<Result<String, String>>,
    },
    /// Replace the PPPoE/RCI configuration.
    Reload {
        config: Box<Config>,
        reply: Sender<Result<(), String>>,
    },
    Shutdown,
}

pub fn spawn(
    config: Config,
    logger: Logger,
    snapshot: SharedSnapshot,
    config_path: PathBuf,
    commands: Receiver<WanCommand>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("wan".to_string())
        .spawn(move || Engine::new(config, logger, snapshot, config_path).run(commands))
        .expect("failed to spawn the WAN thread")
}

struct RecoveryJob {
    interface: String,
    next_at: Instant,
    delay: Duration,
    done: bool,
}

struct Engine {
    config: Config,
    logger: Logger,
    snapshot: SharedSnapshot,
    client: Option<RciClient>,
    targets: Vec<Runtime>,
    transient: Vec<Runtime>,
    latest: HashMap<String, RouterInterface>,
    state: StateFile,
    state_path: PathBuf,
    config_path: PathBuf,
    global_classifier: Classifier,
    recovery_jobs: Vec<RecoveryJob>,
    next_scan: Instant,
    last_scan_error: Option<String>,
    last_state_error: Option<String>,
    state_dirty: bool,
}

impl Engine {
    fn new(
        config: Config,
        logger: Logger,
        snapshot: SharedSnapshot,
        config_path: PathBuf,
    ) -> Engine {
        let client = match make_client(&config) {
            Ok(client) => Some(client),
            Err(err) => {
                logger.error(format!("PPPoE watcher disabled: {err}"));
                None
            }
        };
        let state_path = PathBuf::from(&config.state_file);
        let state = match StateFile::load(&state_path) {
            Ok(state) => state,
            Err(err) => {
                logger.error(format!(
                    "{err}; starting with an empty state file and NOT recovering anything"
                ));
                StateFile::default()
            }
        };
        let global_classifier =
            Classifier::new(&config.pppoe.white_cidrs, &config.pppoe.grey_cidrs)
                .unwrap_or_else(|_| Classifier::new(&[], &[]).expect("empty classifier"));
        let mut targets = Vec::new();
        match config.pppoe_targets() {
            Ok(settings) => {
                for setting in settings {
                    match TargetSpec::from_settings(&setting) {
                        Ok(spec) => targets.push(Runtime::new(spec)),
                        Err(err) => logger.error(format!("pppoe target skipped: {err}")),
                    }
                }
            }
            Err(err) => logger.error(format!("PPPoE configuration is invalid: {err}")),
        }
        let mut recovery_jobs = Vec::new();
        for pending in &state.pending_down {
            recovery_jobs.push(RecoveryJob {
                interface: pending.interface.clone(),
                next_at: Instant::now(),
                delay: HUNT_SCAN,
                done: false,
            });
        }
        if !recovery_jobs.is_empty() {
            logger.warn(format!(
                "recovering {} interface(s) previously taken down by wolguard; \
                 other down interfaces are left alone",
                recovery_jobs.len()
            ));
        }
        // Observations from the previous run are kept for diagnostics only.
        // They deliberately do not suppress the first-observation hunt: the
        // user wants a grey address fixed when the daemon is started/enabled.
        Engine {
            config,
            logger,
            snapshot,
            client,
            targets,
            transient: Vec::new(),
            latest: HashMap::new(),
            state,
            state_path,
            config_path,
            global_classifier,
            recovery_jobs,
            next_scan: Instant::now(),
            last_scan_error: None,
            last_state_error: None,
            state_dirty: false,
        }
    }

    fn run(&mut self, commands: Receiver<WanCommand>) {
        self.logger.info(format!(
            "PPPoE watcher started: {} target(s), RCI {}",
            self.targets.len(),
            self.config.rci.url
        ));
        loop {
            if crate::stop_requested() {
                self.shutdown_recover();
                return;
            }
            while let Ok(command) = commands.try_recv() {
                match command {
                    WanCommand::Shutdown => {
                        self.shutdown_recover();
                        return;
                    }
                    WanCommand::Scan { reply } => {
                        let scan = self.scan(Instant::now());
                        let statuses = self.build_statuses(Instant::now());
                        self.publish(Instant::now());
                        let _ = reply.send(match scan {
                            Ok(_) => Ok(statuses),
                            Err(err) => Err(err),
                        });
                    }
                    WanCommand::Restart {
                        interface,
                        force,
                        oneshot,
                        dry_run,
                        reply,
                    } => {
                        let result = self.manual_restart(&interface, force, oneshot, dry_run);
                        self.publish(Instant::now());
                        let _ = reply.send(result);
                    }
                    WanCommand::Reload { config, reply } => {
                        let result = self.reload(*config);
                        let _ = reply.send(result);
                    }
                }
            }

            let now = Instant::now();
            self.run_recovery(now);
            if now >= self.next_scan && self.should_scan() {
                self.scan_and_apply(now);
            }
            self.run_actions(now);
            self.publish(now);
            if self.state_dirty {
                self.save_state();
            }
            thread::sleep(TICK);
        }
    }

    fn should_scan(&self) -> bool {
        !self.targets.is_empty() || !self.transient.is_empty() || !self.recovery_jobs.is_empty()
    }

    fn scan_and_apply(&mut self, now: Instant) {
        match self.scan(now) {
            Ok(interfaces) => {
                if self.last_scan_error.take().is_some() {
                    self.logger.info("RCI is reachable again");
                }
                self.apply_observations(&interfaces, now);
            }
            Err(err) => {
                if self.last_scan_error.as_deref() != Some(err.as_str()) {
                    self.logger.error(format!("PPPoE scan failed: {err}"));
                    self.last_scan_error = Some(err);
                }
                self.next_scan = now + MAX_SCAN;
            }
        }
    }

    fn scan(&mut self, now: Instant) -> Result<Vec<RouterInterface>, String> {
        let interfaces = match self.list_pppoe() {
            Ok(interfaces) => interfaces,
            Err(RciError::Unauthorized) if self.config.rci.auto_token => {
                match self.bootstrap_token() {
                    Ok(()) => self.list_pppoe().map_err(|err| err.to_string())?,
                    Err(err) => {
                        return Err(format!(
                            "RCI requires an access token and automatic bootstrap failed: \
                             {err}; run `wolguard rci token` or set rci.token"
                        ))
                    }
                }
            }
            Err(err) => return Err(err.to_string()),
        };
        self.latest = interfaces
            .iter()
            .map(|interface| (interface.id.clone(), interface.clone()))
            .collect();
        self.next_scan = now + self.scan_interval();
        Ok(interfaces)
    }

    fn list_pppoe(&mut self) -> Result<Vec<RouterInterface>, RciError> {
        match self.client.as_mut() {
            Some(client) => client.list_pppoe(),
            None => Err(RciError::Io("RCI client is not configured".to_string())),
        }
    }

    fn bootstrap_token(&mut self) -> Result<(), String> {
        let token = crate::rci::generate_token(&self.config.rci.ndmc_path)?;
        if let Some(client) = self.client.as_mut() {
            client.set_token(token.clone());
        }
        self.config.rci.token = token.clone();
        let saved = ConfigDoc::load(&self.config_path).and_then(|mut doc| {
            doc.set_rci(&RciEdit {
                url: self.config.rci.url.clone(),
                token: token.clone(),
                timeout_ms: self.config.rci.timeout_ms,
                auto_token: self.config.rci.auto_token,
                ndmc_path: self.config.rci.ndmc_path.clone(),
            })?;
            doc.save(false)
        });
        match saved {
            Ok(()) => self
                .logger
                .info("generated and saved a new Keenetic RCI access token"),
            Err(err) => self.logger.warn(format!(
                "generated an RCI access token but could not save it to {}: {err}",
                self.config_path.display()
            )),
        }
        Ok(())
    }

    fn scan_interval(&self) -> Duration {
        if self
            .targets
            .iter()
            .chain(self.transient.iter())
            .any(|runtime| runtime.is_hunting())
            || !self.recovery_jobs.is_empty()
        {
            return HUNT_SCAN;
        }
        let mut minimum = self.config.pppoe.poll_secs.max(1);
        for target in &self.config.pppoe.targets {
            if let Some(poll) = target.poll_secs {
                minimum = minimum.min(poll.max(1));
            }
        }
        Duration::from_secs(minimum).max(Duration::from_secs(5))
    }

    fn apply_observations(&mut self, interfaces: &[RouterInterface], now: Instant) {
        for interface in interfaces {
            self.store_observation(interface);
        }
        let by_id: HashMap<&str, &RouterInterface> = interfaces
            .iter()
            .map(|interface| (interface.id.as_str(), interface))
            .collect();

        for runtime in self.targets.iter_mut().chain(self.transient.iter_mut()) {
            let Some(interface) = by_id.get(runtime.interface()).copied() else {
                continue;
            };
            let observation = Observation::from_interface(interface);
            match runtime.phase() {
                Phase::Idle | Phase::WaitingRetry => {
                    let result = runtime.observe(&observation, now);
                    if result.started_hunt {
                        let status = runtime.status(now);
                        self.logger.info(format!(
                            "{}: {} is {} ({}); starting hunt",
                            runtime.interface(),
                            format_address(observation.address),
                            class_label(status.class),
                            status.class_reason.as_deref().unwrap_or("classification")
                        ));
                    } else if result.hunt_finished {
                        let status = runtime.status(now);
                        if status.class == Some(IpClass::White) {
                            self.logger.info(format!(
                                "{}: {} is white ({}); hunt complete",
                                runtime.interface(),
                                format_address(observation.address),
                                status.class_reason.as_deref().unwrap_or("public")
                            ));
                        }
                    } else if result.session_changed {
                        let status = runtime.status(now);
                        if status.class.is_some() {
                            self.logger.info(format!(
                                "{}: address changed to {} ({})",
                                runtime.interface(),
                                format_address(observation.address),
                                status.class_reason.as_deref().unwrap_or("unknown")
                            ));
                        }
                    }
                }
                Phase::WaitingAddress => {
                    let result = runtime.observe_hunt(&observation, now);
                    if result.hunt_finished {
                        let status = runtime.status(now);
                        if status.class == Some(IpClass::White) {
                            self.logger.info(format!(
                                "{}: {} is white ({}); hunt complete",
                                runtime.interface(),
                                format_address(observation.address),
                                status.class_reason.as_deref().unwrap_or("public")
                            ));
                        } else if status.hunting {
                            self.logger.warn(format!(
                                "{}: still {} after the restart ({}); will retry",
                                runtime.interface(),
                                class_label(status.class),
                                status.class_reason.as_deref().unwrap_or("classification")
                            ));
                        } else {
                            self.logger.info(format!(
                                "{}: one-shot test complete; the address is still {} ({})",
                                runtime.interface(),
                                class_label(status.class),
                                status.class_reason.as_deref().unwrap_or("classification")
                            ));
                        }
                    }
                }
                Phase::Downing | Phase::BringingUp | Phase::Recovering => {}
            }
        }
    }

    fn store_observation(&mut self, interface: &RouterInterface) {
        let classification = interface
            .address
            .map(|address| self.global_classifier.classify(address));
        let stored = StoredObservation {
            interface: interface.id.clone(),
            address: interface.address.map(|address| address.to_string()),
            uptime: interface.uptime,
            last_change: interface.last_change.clone(),
            class: classification
                .as_ref()
                .map(|classification| classification.class.label().to_string()),
            class_reason: classification
                .as_ref()
                .map(|classification| classification.reason.clone()),
            updated_unix: unix_now(),
        };
        let existing = self
            .state
            .observations
            .iter()
            .find(|entry| entry.interface == interface.id);
        // `uptime` ticks on every scan; do not rewrite the state file (USB
        // flash) just for that. Address/class changes and session-change
        // evidence are the useful persisted observations.
        let changed = existing.map_or(true, |old| {
            old.address != stored.address
                || old.last_change != stored.last_change
                || old.class != stored.class
                || old.class_reason != stored.class_reason
        });
        if changed {
            self.state.set_observation(stored);
            self.state_dirty = true;
        }
    }

    fn run_actions(&mut self, now: Instant) {
        let mut index = 0;
        while index < self.targets.len() {
            let action = self.targets[index].next_action(now);
            self.perform_action(index, false, action, now);
            index += 1;
        }
        let mut index = 0;
        while index < self.transient.len() {
            let action = self.transient[index].next_action(now);
            self.perform_action(index, true, action, now);
            index += 1;
        }
        self.transient
            .retain(|runtime| runtime.phase() != Phase::Idle || runtime.is_hunting());
    }

    fn perform_action(&mut self, index: usize, transient: bool, action: Action, now: Instant) {
        if action == Action::None {
            return;
        }
        let interface = if transient {
            self.transient[index].interface().to_string()
        } else {
            self.targets[index].interface().to_string()
        };
        match action {
            Action::IssueDown => {
                // The runtime now owns this interface's down/up cycle; a
                // recovery job from an earlier disable/reload must not race it.
                self.recovery_jobs.retain(|job| job.interface != interface);
                if !self.state.is_pending_down(&interface) {
                    self.state
                        .mark_pending_down(&interface, "wolguard restart in progress");
                    self.state_dirty = true;
                    if !self.save_state() {
                        let runtime = if transient {
                            &mut self.transient[index]
                        } else {
                            &mut self.targets[index]
                        };
                        runtime.mark_down(false, now);
                        self.logger.error(format!(
                            "{}: refusing to take the interface down because the recovery \
                             marker could not be saved",
                            interface
                        ));
                        return;
                    }
                }
                let result = self.set_up(&interface, false);
                let (restart_count, max_attempts) = {
                    let runtime = if transient {
                        &mut self.transient[index]
                    } else {
                        &mut self.targets[index]
                    };
                    runtime.mark_down(result.is_ok(), now);
                    let status = runtime.status(now);
                    (
                        status.restart_count,
                        self.config.pppoe.max_restarts_per_hour,
                    )
                };
                match result {
                    Ok(()) => self.logger.info(format!(
                        "{}: restart {}/{} (down)",
                        interface, restart_count, max_attempts
                    )),
                    Err(err) => {
                        self.logger.error(format!(
                            "{}: cannot take the interface down: {err}; scheduling recovery",
                            interface
                        ));
                        self.schedule_recovery(&interface, now);
                    }
                }
            }
            Action::IssueUp => {
                let recovering = if transient {
                    self.transient[index].phase()
                } else {
                    self.targets[index].phase()
                } == Phase::Recovering;
                let result = self.set_up(&interface, true);
                let success = result.is_ok();
                let settle = {
                    let runtime = if transient {
                        &mut self.transient[index]
                    } else {
                        &mut self.targets[index]
                    };
                    if recovering {
                        runtime.mark_recovered(success, now);
                    } else {
                        runtime.mark_up(success, now);
                    }
                    runtime.spec().settle.as_secs()
                };
                match result {
                    Ok(()) => {
                        self.state.clear_pending_down(&interface);
                        self.state_dirty = true;
                        self.logger.info(format!(
                            "{}: up, waiting for an address (up to {} s)",
                            interface, settle
                        ));
                    }
                    Err(err) => {
                        self.logger.error(format!(
                            "{}: cannot bring the interface up: {err}; retrying",
                            interface
                        ));
                    }
                }
            }
            Action::CheckAddress => {
                let observation = match self.fresh_observation(&interface, now) {
                    Ok(Some(interface)) => Observation::from_interface(&interface),
                    Ok(None) => Observation {
                        usable: false,
                        address: None,
                        uptime: None,
                        last_change: None,
                    },
                    Err(err) => {
                        self.logger.error(format!(
                            "{}: cannot read the address back: {err}",
                            interface
                        ));
                        return;
                    }
                };
                let (finished, hunting, class, reason) = {
                    let runtime = if transient {
                        &mut self.transient[index]
                    } else {
                        &mut self.targets[index]
                    };
                    let result = runtime.observe_hunt(&observation, now);
                    let status = runtime.status(now);
                    (
                        result.hunt_finished,
                        status.hunting,
                        status.class,
                        status
                            .class_reason
                            .unwrap_or_else(|| "no classification".to_string()),
                    )
                };
                if finished {
                    if class == Some(IpClass::White) {
                        self.state.clear_pending_down(&interface);
                        self.state_dirty = true;
                        self.logger.info(format!(
                            "{}: new address {} is white ({}); hunt complete",
                            interface,
                            format_address(observation.address),
                            reason
                        ));
                    } else if hunting {
                        self.logger.warn(format!(
                            "{}: still {} ({}); another attempt will follow",
                            interface,
                            class_label(class),
                            reason
                        ));
                    } else {
                        self.logger.info(format!(
                            "{}: one-shot test complete; the address is still {} ({})",
                            interface,
                            class_label(class),
                            reason
                        ));
                    }
                }
            }
            Action::None => {}
        }
    }

    fn set_up(&mut self, interface: &str, up: bool) -> Result<(), String> {
        let Some(client) = self.client.as_mut() else {
            return Err("RCI client is not configured".to_string());
        };
        client
            .set_interface_up(interface, up)
            .map_err(|err| err.to_string())
    }

    fn fresh_observation(
        &mut self,
        interface: &str,
        now: Instant,
    ) -> Result<Option<RouterInterface>, String> {
        let interfaces = self.scan(now)?;
        Ok(interfaces
            .into_iter()
            .find(|candidate| candidate.id == interface))
    }

    fn schedule_recovery(&mut self, interface: &str, now: Instant) {
        if let Some(job) = self
            .recovery_jobs
            .iter_mut()
            .find(|job| job.interface == interface)
        {
            job.next_at = now;
            job.delay = HUNT_SCAN;
            job.done = false;
            return;
        }
        self.recovery_jobs.push(RecoveryJob {
            interface: interface.to_string(),
            next_at: now,
            delay: HUNT_SCAN,
            done: false,
        });
    }

    fn run_recovery(&mut self, now: Instant) {
        if self.recovery_jobs.is_empty() {
            return;
        }
        let mut index = 0;
        while index < self.recovery_jobs.len() {
            let due = self.recovery_jobs[index].next_at <= now;
            if !due {
                index += 1;
                continue;
            }
            let interface = self.recovery_jobs[index].interface.clone();
            let result = self.set_up(&interface, true);
            match result {
                Ok(()) => {
                    self.state.clear_pending_down(&interface);
                    self.state_dirty = true;
                    self.recovery_jobs[index].done = true;
                    self.logger.info(format!(
                        "{}: interface restored after an interrupted restart",
                        interface
                    ));
                }
                Err(err) => {
                    let delay = (self.recovery_jobs[index].delay * 2).min(MAX_SCAN);
                    self.recovery_jobs[index].delay = delay;
                    self.recovery_jobs[index].next_at = now + delay;
                    self.logger.error(format!(
                        "{}: recovery failed ({err}); retrying in {} s",
                        interface,
                        delay.as_secs()
                    ));
                }
            }
            index += 1;
        }
        self.recovery_jobs.retain(|job| !job.done);
    }

    fn shutdown_recover(&mut self) {
        if self.state.pending_down.is_empty() {
            return;
        }
        self.logger
            .info("restoring interfaces taken down by wolguard before exit");
        let interfaces: Vec<String> = self
            .state
            .pending_down
            .iter()
            .map(|entry| entry.interface.clone())
            .collect();
        for interface in interfaces {
            let mut restored = false;
            for _ in 0..3 {
                if self.set_up(&interface, true).is_ok() {
                    restored = true;
                    break;
                }
                thread::sleep(Duration::from_millis(300));
            }
            if restored {
                self.state.clear_pending_down(&interface);
                self.logger.info(format!("{interface}: restored"));
            } else {
                self.logger.error(format!(
                    "{interface}: cannot restore before exit; the next start will retry"
                ));
            }
        }
        self.state_dirty = true;
        self.save_state();
    }

    fn manual_restart(
        &mut self,
        interface: &str,
        force: bool,
        oneshot: bool,
        dry_run: bool,
    ) -> Result<String, String> {
        let now = Instant::now();
        if self.latest.is_empty() {
            let _ = self.scan(now);
        }
        let current = self
            .latest
            .get(interface)
            .cloned()
            .or_else(|| self.fresh_observation(interface, now).ok().flatten())
            .ok_or_else(|| format!("interface {interface:?} not found in RCI"))?;
        let observation = Observation::from_interface(&current);
        let classification = current
            .address
            .map(|address| self.global_classifier.classify(address));
        let report = format!(
            "{}: {} is {} ({})",
            interface,
            format_address(observation.address),
            classification
                .as_ref()
                .map(|classification| class_label(Some(classification.class)))
                .unwrap_or("unknown"),
            classification
                .as_ref()
                .map(|classification| classification.reason.as_str())
                .unwrap_or("no classification")
        );
        if dry_run {
            return Ok(format!("{report}; dry run, no restart"));
        }

        // A manual cycle owns the interface; drop a pending recovery retry so
        // it cannot bring the interface up in the middle of the cycle.
        self.recovery_jobs.retain(|job| job.interface != interface);

        let target_index = self
            .targets
            .iter()
            .position(|runtime| runtime.interface() == interface);
        let watched_active = target_index
            .map(|index| self.targets[index].spec().enabled)
            .unwrap_or(false);
        if target_index.is_none() {
            let spec = self.transient_spec(interface)?;
            self.transient.push(Runtime::new(spec));
        }
        let runtime = if let Some(index) = target_index {
            &mut self.targets[index]
        } else {
            self.transient
                .last_mut()
                .expect("transient runtime was just inserted")
        };
        let oneshot = oneshot || !watched_active;
        runtime.request_manual(now, force, oneshot)?;
        Ok(format!("{report}; restart started"))
    }

    fn transient_spec(&self, interface: &str) -> Result<TargetSpec, String> {
        let pppoe = &self.config.pppoe;
        let classifier = Classifier::new(&pppoe.white_cidrs, &pppoe.grey_cidrs)?;
        Ok(TargetSpec {
            interface: interface.to_string(),
            enabled: false,
            trigger: Trigger::OnChange,
            down: Duration::from_secs(pppoe.down_secs),
            settle: Duration::from_secs(pppoe.settle_secs),
            retry: Duration::from_secs(pppoe.retry_secs),
            max_restarts_per_hour: pppoe.max_restarts_per_hour,
            classifier,
        })
    }

    fn reload(&mut self, config: Config) -> Result<(), String> {
        config.validate()?;
        let settings = config.pppoe_targets()?;
        let now = Instant::now();
        let mut old_targets = std::mem::take(&mut self.targets);
        let mut new_targets = Vec::with_capacity(settings.len());
        for setting in settings {
            let spec = TargetSpec::from_settings(&setting)?;
            if let Some(index) = old_targets
                .iter()
                .position(|runtime| runtime.interface() == spec.interface)
            {
                let mut runtime = old_targets.remove(index);
                let interface = runtime.interface().to_string();
                runtime.replace_spec(spec, now);
                // Disabling a target (or the global PPPoE switch) while a
                // restart marker is pending must not leave the interface
                // down: hand it to the recovery path.
                if self.state.is_pending_down(&interface)
                    && (!runtime.spec().enabled || !runtime.is_hunting())
                {
                    self.schedule_recovery(&interface, now);
                }
                new_targets.push(runtime);
            } else {
                new_targets.push(Runtime::new(spec));
            }
        }
        for mut removed in old_targets {
            let interface = removed.interface().to_string();
            removed.disable();
            if self.state.is_pending_down(&interface) {
                self.schedule_recovery(&interface, now);
            }
        }
        self.targets = new_targets;

        let state_path = PathBuf::from(&config.state_file);
        if state_path != self.state_path {
            // Move the state (including a pending recovery marker) before
            // switching; otherwise a restart after a state_file change would
            // lose the marker and leave an owned interface down.
            match self.state.save(&state_path) {
                Ok(()) => {
                    let old = std::mem::replace(&mut self.state_path, state_path);
                    let _ = std::fs::remove_file(&old);
                    self.logger.info(format!(
                        "PPPoE state moved to {}",
                        self.state_path.display()
                    ));
                }
                Err(err) => {
                    self.logger.error(format!(
                        "cannot move PPPoE state to {}: {err}; keeping {}",
                        state_path.display(),
                        self.state_path.display()
                    ));
                }
            }
        }
        self.config = config;
        match make_client(&self.config) {
            Ok(client) => self.client = Some(client),
            Err(err) => {
                self.client = None;
                self.logger
                    .error(format!("RCI configuration is invalid: {err}"));
            }
        }
        self.global_classifier = Classifier::new(
            &self.config.pppoe.white_cidrs,
            &self.config.pppoe.grey_cidrs,
        )
        .unwrap_or_else(|_| Classifier::new(&[], &[]).expect("empty classifier"));
        self.next_scan = now;
        self.logger.info(format!(
            "PPPoE configuration reloaded: {} target(s)",
            self.targets.len()
        ));
        self.publish(now);
        Ok(())
    }

    fn build_statuses(&self, now: Instant) -> Vec<WanStatus> {
        let mut statuses = Vec::new();
        let mut known = HashSet::new();
        let mut interfaces: Vec<&RouterInterface> = self.latest.values().collect();
        interfaces.sort_by(|a, b| a.id.cmp(&b.id));
        for interface in interfaces {
            known.insert(interface.id.clone());
            let runtime = self
                .targets
                .iter()
                .find(|runtime| runtime.interface() == interface.id)
                .or_else(|| {
                    self.transient
                        .iter()
                        .find(|runtime| runtime.interface() == interface.id)
                });
            let watched = self
                .targets
                .iter()
                .any(|runtime| runtime.interface() == interface.id);
            statuses.push(self.status_for(interface, runtime, watched, now));
        }
        for runtime in &self.targets {
            if known.contains(runtime.interface()) {
                continue;
            }
            statuses.push(WanStatus {
                id: runtime.interface().to_string(),
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
                enabled: runtime.spec().enabled,
                hunting: runtime.is_hunting(),
                phase: runtime.phase().label().to_string(),
                restart_count: runtime.status(now).restart_count,
                last_restart: runtime.status(now).last_restart,
                next_action_in_secs: runtime
                    .status(now)
                    .next_action_in
                    .map(|duration| duration.as_secs()),
                last_error: Some("interface not found in RCI".to_string()),
                recovery_pending: self.state.is_pending_down(runtime.interface()),
            });
        }
        statuses.sort_by(|a, b| a.id.cmp(&b.id));
        statuses
    }

    fn status_for(
        &self,
        interface: &RouterInterface,
        runtime: Option<&Runtime>,
        watched: bool,
        now: Instant,
    ) -> WanStatus {
        let status = runtime.map(|runtime| runtime.status(now));
        let classification = interface.address.map(|address| match runtime {
            Some(runtime) => runtime.spec().classifier.classify(address),
            None => self.global_classifier.classify(address),
        });
        let class = classification
            .as_ref()
            .map(|value| value.class)
            .or_else(|| status.as_ref().and_then(|status| status.class));
        let class_reason = classification.map(|value| value.reason).or_else(|| {
            status
                .as_ref()
                .and_then(|status| status.class_reason.clone())
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
            class,
            class_reason,
            uptime_secs: interface.uptime,
            watched,
            enabled: status
                .as_ref()
                .map(|status| status.enabled)
                .unwrap_or(false),
            hunting: status
                .as_ref()
                .map(|status| status.hunting)
                .unwrap_or(false),
            phase: status
                .as_ref()
                .map(|status| status.phase.label().to_string())
                .unwrap_or_else(|| "idle".to_string()),
            restart_count: status
                .as_ref()
                .map(|status| status.restart_count)
                .unwrap_or(0),
            last_restart: status.as_ref().and_then(|status| status.last_restart),
            next_action_in_secs: status
                .as_ref()
                .and_then(|status| status.next_action_in)
                .map(|duration| duration.as_secs()),
            last_error: status.and_then(|status| status.last_error),
            recovery_pending: self.state.is_pending_down(&interface.id),
        }
    }

    fn session_signals(&self) -> Vec<String> {
        let mut signals = vec!["address".to_string()];
        if self
            .latest
            .values()
            .any(|interface| interface.uptime.is_some())
        {
            signals.push("uptime".to_string());
        }
        if self
            .latest
            .values()
            .any(|interface| interface.last_change.is_some())
        {
            signals.push("last-change".to_string());
        }
        if !self.targets.is_empty() {
            signals.push("down-up".to_string());
        }
        signals
    }

    fn publish(&self, now: Instant) {
        let statuses = self.build_statuses(now);
        let signals = self.session_signals();
        let mut snapshot: MutexGuard<'_, Snapshot> = self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        snapshot.wan = statuses;
        snapshot.session_signals = signals;
        snapshot.wol_enabled = self.config.wol_enabled;
        snapshot.last_update = SystemTime::now();
    }

    fn save_state(&mut self) -> bool {
        self.state_dirty = false;
        match self.state.save(&self.state_path) {
            Ok(()) => {
                self.last_state_error = None;
                true
            }
            Err(err) => {
                if self.last_state_error.as_deref() != Some(err.as_str()) {
                    self.logger.error(format!(
                        "cannot save PPPoE recovery state {}: {err}",
                        self.state_path.display()
                    ));
                    self.last_state_error = Some(err);
                }
                false
            }
        }
    }
}

fn make_client(config: &Config) -> Result<RciClient, String> {
    RciClient::from_url(
        &config.rci.url,
        &config.rci.token,
        Duration::from_millis(config.rci.timeout_ms),
    )
}

fn class_label(class: Option<IpClass>) -> &'static str {
    class.map(IpClass::label).unwrap_or("unknown")
}

fn format_address(address: Option<std::net::Ipv4Addr>) -> String {
    address
        .map(|address| address.to_string())
        .unwrap_or_else(|| "no address".to_string())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

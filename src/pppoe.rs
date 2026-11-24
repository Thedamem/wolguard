//! PPPoE white-IP watchdog state machine.
//!
//! The state machine is pure: it receives observations and a monotonic clock,
//! returns the next action, and is told whether the router accepted it. The
//! WAN thread in the daemon owns the RCI client, the persistent marker and the
//! logging; this module can be tested with a fake clock and no router.

use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::{PppoeTargetSettings, Trigger};
use crate::ipclass::{Classification, Classifier, IpClass};
use crate::rci::RouterInterface;

const HOUR: Duration = Duration::from_secs(3600);
const ADDRESS_POLL: Duration = Duration::from_secs(2);
const RECOVERY_RETRY: Duration = Duration::from_secs(2);
const MAX_RECOVERY_RETRY: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    pub usable: bool,
    pub address: Option<Ipv4Addr>,
    pub uptime: Option<u64>,
    pub last_change: Option<String>,
}

impl Observation {
    pub fn from_interface(interface: &RouterInterface) -> Observation {
        Observation {
            usable: interface.is_usable(),
            address: interface.address,
            uptime: interface.uptime,
            last_change: interface.last_change.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Idle,
    Downing,
    BringingUp,
    WaitingAddress,
    WaitingRetry,
    Recovering,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Downing => "down",
            Phase::BringingUp => "up",
            Phase::WaitingAddress => "waiting-ip",
            Phase::WaitingRetry => "waiting-retry",
            Phase::Recovering => "recovering",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    IssueDown,
    IssueUp,
    CheckAddress,
}

#[derive(Clone, Debug, Default)]
pub struct ObserveResult {
    pub started_hunt: bool,
    pub hunt_finished: bool,
    pub session_changed: bool,
    pub classified: Option<Classification>,
}

#[derive(Clone, Debug)]
pub struct TargetSpec {
    pub interface: String,
    pub enabled: bool,
    pub trigger: Trigger,
    pub down: Duration,
    pub settle: Duration,
    pub retry: Duration,
    pub max_restarts_per_hour: u32,
    pub classifier: Classifier,
}

impl TargetSpec {
    pub fn from_settings(settings: &PppoeTargetSettings) -> Result<TargetSpec, String> {
        Ok(TargetSpec {
            interface: settings.interface.clone(),
            enabled: settings.enabled,
            trigger: settings.trigger,
            down: settings.down,
            settle: settings.settle,
            retry: settings.retry,
            max_restarts_per_hour: settings.max_restarts_per_hour,
            classifier: settings.classifier()?,
        })
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeStatus {
    pub phase: Phase,
    pub hunting: bool,
    pub class: Option<IpClass>,
    pub class_reason: Option<String>,
    pub restart_count: u32,
    pub last_restart: Option<SystemTime>,
    pub next_action_in: Option<Duration>,
    pub last_error: Option<String>,
    pub enabled: bool,
}

pub struct Runtime {
    spec: TargetSpec,
    last: Option<Observation>,
    classified: Option<Classification>,
    started: bool,
    saw_down: bool,
    hunting: bool,
    oneshot: bool,
    phase: Phase,
    next_action_at: Option<Instant>,
    address_deadline: Option<Instant>,
    attempts: VecDeque<Instant>,
    restart_count: u32,
    last_restart: Option<SystemTime>,
    last_error: Option<String>,
    force_next: bool,
    suppress_auto: bool,
    recovery_delay: Duration,
}

impl Runtime {
    pub fn new(spec: TargetSpec) -> Runtime {
        Runtime {
            spec,
            last: None,
            classified: None,
            started: false,
            saw_down: false,
            hunting: false,
            oneshot: false,
            phase: Phase::Idle,
            next_action_at: None,
            address_deadline: None,
            attempts: VecDeque::new(),
            restart_count: 0,
            last_restart: None,
            last_error: None,
            force_next: false,
            suppress_auto: false,
            recovery_delay: RECOVERY_RETRY,
        }
    }

    pub fn interface(&self) -> &str {
        &self.spec.interface
    }

    pub fn spec(&self) -> &TargetSpec {
        &self.spec
    }

    pub fn replace_spec(&mut self, spec: TargetSpec, now: Instant) {
        let was_enabled = self.spec.enabled;
        self.spec = spec;
        if !was_enabled && self.spec.enabled {
            self.enable(now);
        } else if !self.spec.enabled {
            self.disable();
        }
    }

    pub fn enable(&mut self, _now: Instant) {
        self.spec.enabled = true;
        self.started = false;
        self.saw_down = false;
        self.hunting = false;
        self.oneshot = false;
        self.suppress_auto = false;
        self.phase = Phase::Idle;
        self.next_action_at = None;
        self.address_deadline = None;
    }

    pub fn disable(&mut self) {
        self.spec.enabled = false;
        self.hunting = false;
        self.oneshot = false;
        self.phase = Phase::Idle;
        self.next_action_at = None;
        self.address_deadline = None;
    }

    /// Prime the last observation without starting a hunt. Used by tests and
    /// by the WAN thread when it restores observations from the state file for
    /// display. It does not suppress the first-observation hunt: the WAN thread
    /// does not call this for a fresh target.
    #[cfg(test)]
    pub fn prime(&mut self, observation: Observation, _now: Instant) {
        if observation.usable {
            self.started = true;
            self.classified = observation
                .address
                .map(|address| self.spec.classifier.classify(address));
        }
        self.last = Some(observation);
    }

    pub fn is_hunting(&self) -> bool {
        self.hunting
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Feed one observation. Starts a hunt when the policy says so.
    pub fn observe(&mut self, observation: &Observation, now: Instant) -> ObserveResult {
        let mut result = ObserveResult::default();

        if !observation.usable {
            self.saw_down = true;
            self.last = Some(observation.clone());
            return result;
        }

        let previous = self.last.clone();
        let first = !self.started;
        self.started = true;

        let session_changed = self.session_changed(previous.as_ref(), observation);
        result.session_changed = session_changed;
        self.saw_down = false;
        self.last = Some(observation.clone());

        let classification = observation
            .address
            .map(|address| self.spec.classifier.classify(address));
        if let Some(classification) = &classification {
            result.classified = Some(classification.clone());
            self.classified = Some(classification.clone());
        }

        match classification {
            Some(classification) if classification.class == IpClass::White => {
                if self.hunting {
                    self.hunting = false;
                    self.oneshot = false;
                    self.phase = Phase::Idle;
                    self.next_action_at = None;
                    self.address_deadline = None;
                    self.last_error = None;
                    result.hunt_finished = true;
                }
            }
            Some(_) => {
                if self.hunting {
                    return result;
                }
                if self.suppress_auto && session_changed {
                    self.suppress_auto = false;
                }
                let policy_allows = if self.suppress_auto {
                    session_changed
                } else {
                    first || session_changed || self.spec.trigger == Trigger::Always
                };
                if self.spec.enabled && policy_allows {
                    if let Err(err) = self.start_hunt(now, false, false) {
                        self.last_error = Some(err);
                    } else {
                        result.started_hunt = true;
                    }
                }
            }
            None => {}
        }
        result
    }

    fn session_changed(&self, previous: Option<&Observation>, current: &Observation) -> bool {
        if self.saw_down {
            return true;
        }
        let Some(previous) = previous else {
            return false;
        };
        if previous.address != current.address && previous.address.is_some() {
            return true;
        }
        if let (Some(previous_uptime), Some(current_uptime)) = (previous.uptime, current.uptime) {
            if current_uptime < previous_uptime {
                return true;
            }
        }
        if let (Some(previous_change), Some(current_change)) =
            (&previous.last_change, &current.last_change)
        {
            if previous_change != current_change {
                return true;
            }
        }
        false
    }

    /// Feed an observation while waiting for an address after an up.
    pub fn observe_hunt(&mut self, observation: &Observation, now: Instant) -> ObserveResult {
        let mut result = ObserveResult::default();
        self.last = Some(observation.clone());
        if observation.usable {
            let classification = observation
                .address
                .map(|address| self.spec.classifier.classify(address));
            if let Some(classification) = &classification {
                result.classified = Some(classification.clone());
                self.classified = Some(classification.clone());
                if classification.class == IpClass::White {
                    self.hunting = false;
                    self.oneshot = false;
                    self.phase = Phase::Idle;
                    self.next_action_at = None;
                    self.address_deadline = None;
                    self.last_error = None;
                    result.hunt_finished = true;
                    return result;
                }
            }
        }

        let deadline_reached = self
            .address_deadline
            .map(|deadline| now >= deadline)
            .unwrap_or(true);
        if deadline_reached {
            self.finish_attempt(now, false);
            result.hunt_finished = true;
        } else {
            self.next_action_at = Some(now + ADDRESS_POLL);
        }
        result
    }

    /// Start the hunt manually (CLI/TUI restart or test). `oneshot` stops after
    /// the first cycle; the automatic hunt continues on its own.
    pub fn request_manual(
        &mut self,
        now: Instant,
        force: bool,
        oneshot: bool,
    ) -> Result<(), String> {
        if self.hunting || !matches!(self.phase, Phase::Idle) {
            return Err(format!(
                "{}: a restart is already in progress ({})",
                self.spec.interface,
                self.phase.label()
            ));
        }
        if !force {
            self.prune(now);
            if self.attempts.len() as u32 >= self.spec.max_restarts_per_hour {
                let until = self
                    .attempts
                    .front()
                    .copied()
                    .unwrap_or(now)
                    .saturating_duration_since(now);
                return Err(format!(
                    "{}: restart limit reached ({}/hour); next attempt in {} s",
                    self.spec.interface,
                    self.spec.max_restarts_per_hour,
                    until.as_secs().max(1)
                ));
            }
        }
        self.hunting = true;
        self.oneshot = oneshot;
        self.suppress_auto = oneshot;
        self.force_next = force;
        self.phase = Phase::Downing;
        self.next_action_at = Some(now);
        self.address_deadline = None;
        self.last_error = None;
        Ok(())
    }

    fn start_hunt(&mut self, now: Instant, force: bool, oneshot: bool) -> Result<(), String> {
        if self.hunting {
            return Ok(());
        }
        if !force {
            self.prune(now);
            if self.attempts.len() as u32 >= self.spec.max_restarts_per_hour {
                self.hunting = true;
                self.oneshot = false;
                self.phase = Phase::WaitingRetry;
                let until = self
                    .attempts
                    .front()
                    .copied()
                    .unwrap_or(now)
                    .saturating_duration_since(now);
                self.next_action_at = Some(now + until.max(Duration::from_secs(1)));
                return Err(format!(
                    "restart limit reached ({}/hour); next attempt in {} s",
                    self.spec.max_restarts_per_hour,
                    until.as_secs().max(1)
                ));
            }
        }
        self.hunting = true;
        self.oneshot = oneshot;
        self.suppress_auto = false;
        self.force_next = force;
        self.phase = Phase::Downing;
        self.next_action_at = Some(now);
        self.address_deadline = None;
        Ok(())
    }

    pub fn next_action(&mut self, now: Instant) -> Action {
        let Some(at) = self.next_action_at else {
            return Action::None;
        };
        if now < at {
            return Action::None;
        }
        match self.phase {
            Phase::Idle => Action::None,
            Phase::Downing => Action::IssueDown,
            Phase::BringingUp => Action::IssueUp,
            Phase::WaitingAddress => Action::CheckAddress,
            Phase::WaitingRetry => {
                if !self.hunting {
                    self.phase = Phase::Idle;
                    self.next_action_at = None;
                    return Action::None;
                }
                self.prune(now);
                if !self.force_next && self.attempts.len() as u32 >= self.spec.max_restarts_per_hour
                {
                    if self.oneshot {
                        self.hunting = false;
                        self.phase = Phase::Idle;
                        self.next_action_at = None;
                        return Action::None;
                    }
                    let until = self
                        .attempts
                        .front()
                        .copied()
                        .unwrap_or(now)
                        .saturating_duration_since(now)
                        .max(Duration::from_secs(1));
                    self.next_action_at = Some(now + until);
                    return Action::None;
                }
                self.force_next = false;
                self.phase = Phase::Downing;
                self.next_action_at = Some(now);
                Action::IssueDown
            }
            Phase::Recovering => Action::IssueUp,
        }
    }

    pub fn mark_down(&mut self, success: bool, now: Instant) {
        if !success {
            self.last_error = Some("router did not accept the down request".to_string());
            if self.oneshot {
                self.hunting = false;
                self.phase = Phase::Idle;
                self.next_action_at = None;
            } else {
                self.phase = Phase::WaitingRetry;
                self.next_action_at = Some(now + self.spec.retry);
            }
            return;
        }
        self.last_error = None;
        self.force_next = false;
        self.attempts.push_back(now);
        self.restart_count = self.restart_count.saturating_add(1);
        self.last_restart = Some(SystemTime::now());
        self.phase = Phase::BringingUp;
        self.next_action_at = Some(now + self.spec.down);
    }

    pub fn mark_up(&mut self, success: bool, now: Instant) {
        if success {
            self.last_error = None;
            self.recovery_delay = RECOVERY_RETRY;
            self.phase = Phase::WaitingAddress;
            self.next_action_at = Some(now + ADDRESS_POLL);
            self.address_deadline = Some(now + self.spec.settle);
        } else {
            self.last_error = Some("router did not accept the up request".to_string());
            self.phase = Phase::Recovering;
            self.next_action_at = Some(now + self.recovery_delay);
        }
    }

    pub fn mark_recovered(&mut self, success: bool, now: Instant) {
        if success {
            self.last_error = None;
            self.recovery_delay = RECOVERY_RETRY;
            if self.hunting {
                self.phase = Phase::WaitingAddress;
                self.next_action_at = Some(now + ADDRESS_POLL);
                self.address_deadline = Some(now + self.spec.settle);
            } else {
                self.phase = Phase::Idle;
                self.next_action_at = None;
            }
        } else {
            self.last_error = Some("cannot bring the interface back up".to_string());
            self.phase = Phase::Recovering;
            self.recovery_delay = (self.recovery_delay * 2).min(MAX_RECOVERY_RETRY);
            self.next_action_at = Some(now + self.recovery_delay);
        }
    }

    fn finish_attempt(&mut self, now: Instant, success: bool) {
        if success {
            self.hunting = false;
            self.oneshot = false;
            self.phase = Phase::Idle;
            self.next_action_at = None;
            self.address_deadline = None;
            return;
        }
        if self.oneshot {
            self.hunting = false;
            self.oneshot = false;
            self.phase = Phase::Idle;
            self.next_action_at = None;
        } else {
            self.phase = Phase::WaitingRetry;
            self.next_action_at = Some(now + self.spec.retry);
            // If this attempt used its force budget, clear it for the next one.
            self.force_next = false;
        }
        self.address_deadline = None;
    }

    fn prune(&mut self, now: Instant) {
        while let Some(oldest) = self.attempts.front().copied() {
            if now.saturating_duration_since(oldest) >= HOUR {
                self.attempts.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn status(&self, now: Instant) -> RuntimeStatus {
        RuntimeStatus {
            phase: self.phase,
            hunting: self.hunting,
            class: self
                .classified
                .as_ref()
                .map(|classification| classification.class),
            class_reason: self
                .classified
                .as_ref()
                .map(|classification| classification.reason.clone()),
            restart_count: self.restart_count,
            last_restart: self.last_restart,
            next_action_in: self
                .next_action_at
                .map(|at| at.saturating_duration_since(now)),
            last_error: self.last_error.clone(),
            enabled: self.spec.enabled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(trigger: Trigger) -> TargetSpec {
        TargetSpec {
            interface: "PPPoE0".to_string(),
            enabled: true,
            trigger,
            down: Duration::from_secs(5),
            settle: Duration::from_secs(60),
            retry: Duration::from_secs(45),
            max_restarts_per_hour: 2,
            classifier: Classifier::new(&[], &[]).unwrap(),
        }
    }

    fn grey() -> Observation {
        Observation {
            usable: true,
            address: Some("100.70.12.34".parse().unwrap()),
            uptime: Some(100),
            last_change: None,
        }
    }

    fn white() -> Observation {
        Observation {
            usable: true,
            address: Some("95.31.12.7".parse().unwrap()),
            uptime: Some(200),
            last_change: None,
        }
    }

    fn down() -> Observation {
        Observation {
            usable: false,
            address: None,
            uptime: None,
            last_change: None,
        }
    }

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn on_change_hunts_a_grey_address_on_first_observation() {
        let mut runtime = Runtime::new(spec(Trigger::OnChange));
        let result = runtime.observe(&grey(), t0());
        assert!(result.started_hunt);
        assert_eq!(runtime.phase(), Phase::Downing);
        assert!(runtime.is_hunting());
    }

    #[test]
    fn on_change_does_not_hunt_a_stable_grey_after_a_oneshot() {
        let mut runtime = Runtime::new(spec(Trigger::OnChange));
        let now = t0();
        runtime.prime(grey(), now);
        runtime.request_manual(now, true, true).unwrap();
        runtime.next_action(now);
        runtime.mark_down(true, now);
        runtime.mark_up(true, now);
        runtime.observe_hunt(&grey(), now + Duration::from_secs(61));
        assert!(!runtime.is_hunting());
        assert_eq!(runtime.phase(), Phase::Idle);

        let result = runtime.observe(&grey(), now + Duration::from_secs(70));
        assert!(!result.started_hunt, "stable grey must not re-hunt");
    }

    #[test]
    fn test_cycle_does_not_auto_restart_even_with_always() {
        let mut runtime = Runtime::new(spec(Trigger::Always));
        let now = t0();
        runtime.prime(grey(), now);
        runtime.request_manual(now, true, true).unwrap();
        runtime.mark_down(true, now);
        runtime.mark_up(true, now);
        runtime.observe_hunt(&grey(), now + Duration::from_secs(61));
        assert!(!runtime.is_hunting());

        let result = runtime.observe(&grey(), now + Duration::from_secs(70));
        assert!(
            !result.started_hunt,
            "a manual test must stay a one-shot until the session changes"
        );

        let mut changed = grey();
        changed.uptime = Some(1);
        let result = runtime.observe(&changed, now + Duration::from_secs(80));
        assert!(result.started_hunt, "a session change re-arms the watchdog");
    }

    #[test]
    fn white_address_stops_an_automatic_hunt() {
        let mut runtime = Runtime::new(spec(Trigger::OnChange));
        let now = t0();
        runtime.observe(&grey(), now);
        runtime.mark_down(true, now);
        runtime.mark_up(true, now);
        let result = runtime.observe_hunt(&white(), now + Duration::from_secs(10));
        assert!(result.hunt_finished);
        assert!(!runtime.is_hunting());
        assert_eq!(runtime.phase(), Phase::Idle);
    }

    #[test]
    fn grey_address_after_up_schedules_another_attempt() {
        let mut runtime = Runtime::new(spec(Trigger::OnChange));
        let now = t0();
        runtime.observe(&grey(), now);
        runtime.mark_down(true, now);
        runtime.mark_up(true, now);
        let result = runtime.observe_hunt(&grey(), now + Duration::from_secs(61));
        assert!(result.hunt_finished);
        assert!(runtime.is_hunting());
        assert_eq!(runtime.phase(), Phase::WaitingRetry);
        let status = runtime.status(now + Duration::from_secs(61));
        assert!(status.next_action_in.unwrap() <= Duration::from_secs(45));
    }

    #[test]
    fn uptime_reset_counts_as_a_session_change() {
        let mut runtime = Runtime::new(spec(Trigger::OnChange));
        let now = t0();
        runtime.observe(&white(), now);
        let mut reset = grey();
        reset.uptime = Some(1);
        let result = runtime.observe(&reset, now + Duration::from_secs(10));
        assert!(result.session_changed);
        assert!(result.started_hunt);
    }

    #[test]
    fn observed_down_then_same_grey_counts_as_a_session_change() {
        let mut runtime = Runtime::new(spec(Trigger::OnChange));
        let now = t0();
        runtime.observe(&white(), now);
        runtime.observe(&down(), now + Duration::from_secs(5));
        let result = runtime.observe(&grey(), now + Duration::from_secs(10));
        assert!(result.session_changed);
        assert!(result.started_hunt);
    }

    #[test]
    fn up_failure_enters_recovery_and_recovers() {
        let mut runtime = Runtime::new(spec(Trigger::OnChange));
        let now = t0();
        runtime.observe(&grey(), now);
        runtime.mark_down(true, now);
        runtime.mark_up(false, now);
        assert_eq!(runtime.phase(), Phase::Recovering);
        assert_eq!(
            runtime.next_action(now + Duration::from_secs(1)),
            Action::None
        );
        assert_eq!(
            runtime.next_action(now + Duration::from_secs(2)),
            Action::IssueUp
        );
        runtime.mark_recovered(true, now + Duration::from_secs(2));
        assert_eq!(runtime.phase(), Phase::WaitingAddress);
        let result = runtime.observe_hunt(&white(), now + Duration::from_secs(5));
        assert!(result.hunt_finished);
        assert!(!runtime.is_hunting());
    }

    #[test]
    fn rate_limit_pauses_and_then_resumes() {
        let mut runtime = Runtime::new(spec(Trigger::OnChange));
        let now = t0();
        runtime.observe(&grey(), now);
        runtime.mark_down(true, now);
        runtime.mark_up(true, now);
        runtime.observe_hunt(&grey(), now + Duration::from_secs(61));
        assert_eq!(runtime.phase(), Phase::WaitingRetry);

        // One more attempt is allowed (limit is 2), then the window is full.
        let second = now + Duration::from_secs(106);
        assert_eq!(runtime.next_action(second), Action::IssueDown);
        runtime.mark_down(true, second);
        runtime.mark_up(true, second);
        runtime.observe_hunt(&grey(), second + Duration::from_secs(61));
        assert_eq!(runtime.phase(), Phase::WaitingRetry);

        // The third attempt within the hour must wait.
        let third = second + Duration::from_secs(106);
        assert_eq!(runtime.next_action(third), Action::None);
        let status = runtime.status(third);
        assert!(status.next_action_in.is_some());

        // After the first attempt leaves the one-hour window it may run.
        let after_window = now + HOUR + Duration::from_secs(1);
        assert_eq!(runtime.next_action(after_window), Action::IssueDown);
    }

    #[test]
    fn oneshot_stops_after_one_grey_cycle() {
        let mut runtime = Runtime::new(spec(Trigger::OnChange));
        let now = t0();
        runtime.prime(grey(), now);
        runtime.request_manual(now, true, true).unwrap();
        runtime.mark_down(true, now);
        runtime.mark_up(true, now);
        runtime.observe_hunt(&grey(), now + Duration::from_secs(61));
        assert!(!runtime.is_hunting());
        assert_eq!(runtime.phase(), Phase::Idle);
        assert_eq!(
            runtime.next_action(now + Duration::from_secs(120)),
            Action::None
        );
    }

    #[test]
    fn manual_restart_is_refused_while_busy() {
        let mut runtime = Runtime::new(spec(Trigger::OnChange));
        let now = t0();
        runtime.observe(&grey(), now);
        let err = runtime.request_manual(now, true, true).unwrap_err();
        assert!(err.contains("already in progress"), "{err}");
    }

    #[test]
    fn disabled_target_does_not_start_a_hunt_but_manual_restart_works() {
        let mut runtime = Runtime::new(spec(Trigger::OnChange));
        runtime.disable();
        let now = t0();
        let result = runtime.observe(&grey(), now);
        assert!(!result.started_hunt);
        runtime.request_manual(now, true, true).unwrap();
        assert_eq!(runtime.next_action(now), Action::IssueDown);
    }

    #[test]
    fn enable_after_disable_rearms_the_initial_check() {
        let mut runtime = Runtime::new(spec(Trigger::OnChange));
        let now = t0();
        runtime.disable();
        runtime.observe(&grey(), now);
        runtime.enable(now);
        let result = runtime.observe(&grey(), now + Duration::from_secs(1));
        assert!(result.started_hunt);
    }
}

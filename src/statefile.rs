//! Persistent PPPoE recovery/observation state.
//!
//! The only critical entry is `pending_down`: an interface this daemon took
//! down and has not yet verified back up. It survives SIGKILL and is reconciled
//! at the next daemon start. Nothing else in this file ever causes an
//! interface to be brought up: interfaces that were already down for other
//! reasons are left alone.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const STATE_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingDown {
    pub interface: String,
    pub since_unix: u64,
    #[serde(default)]
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredObservation {
    pub interface: String,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub uptime: Option<u64>,
    #[serde(default)]
    pub last_change: Option<String>,
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub class_reason: Option<String>,
    #[serde(default)]
    pub updated_unix: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub pending_down: Vec<PendingDown>,
    #[serde(default)]
    pub observations: Vec<StoredObservation>,
}

fn default_version() -> u32 {
    STATE_VERSION
}

impl Default for StateFile {
    fn default() -> Self {
        StateFile {
            version: STATE_VERSION,
            pending_down: Vec::new(),
            observations: Vec::new(),
        }
    }
}

impl StateFile {
    pub fn load(path: &Path) -> Result<StateFile, String> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let mut state: StateFile = serde_json::from_str(&text)
                    .map_err(|err| format!("cannot parse state file {}: {err}", path.display()))?;
                if state.version > STATE_VERSION {
                    return Err(format!(
                        "state file {} has version {}, this wolguard supports {STATE_VERSION}",
                        path.display(),
                        state.version
                    ));
                }
                state.version = STATE_VERSION;
                Ok(state)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(StateFile::default()),
            Err(err) => Err(format!("cannot read state file {}: {err}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent).map_err(|err| {
                format!("cannot create state directory {}: {err}", parent.display())
            })?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|err| format!("cannot serialize state: {err}"))?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("wolguard.state");
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let temp = path.with_file_name(format!(".{file_name}.tmp.{}.{nanos}", std::process::id()));
        write_private(&temp, text.as_bytes())?;
        if let Err(err) = fs::rename(&temp, path) {
            let _ = fs::remove_file(&temp);
            return Err(format!(
                "cannot replace state file {}: {err}",
                path.display()
            ));
        }
        Ok(())
    }

    pub fn is_pending_down(&self, interface: &str) -> bool {
        self.pending_down
            .iter()
            .any(|entry| entry.interface == interface)
    }

    pub fn mark_pending_down(&mut self, interface: &str, reason: &str) {
        let since_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        if let Some(entry) = self
            .pending_down
            .iter_mut()
            .find(|entry| entry.interface == interface)
        {
            entry.since_unix = since_unix;
            entry.reason = reason.to_string();
        } else {
            self.pending_down.push(PendingDown {
                interface: interface.to_string(),
                since_unix,
                reason: reason.to_string(),
            });
        }
    }

    pub fn clear_pending_down(&mut self, interface: &str) {
        self.pending_down
            .retain(|entry| entry.interface != interface);
    }

    pub fn set_observation(&mut self, observation: StoredObservation) {
        if let Some(existing) = self
            .observations
            .iter_mut()
            .find(|entry| entry.interface == observation.interface)
        {
            if *existing == observation {
                return;
            }
            *existing = observation;
        } else {
            self.observations.push(observation);
        }
    }
}

fn write_private(path: &Path, contents: &[u8]) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|err| format!("cannot create {}: {err}", path.display()))?;
    file.write_all(contents)
        .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    file.flush()
        .map_err(|err| format!("cannot flush {}: {err}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "wolguard-state-{}-{}-{name}.json",
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn missing_file_is_an_empty_state() {
        let path = temp_path("missing");
        let state = StateFile::load(&path).unwrap();
        assert!(state.pending_down.is_empty());
        assert!(state.observations.is_empty());
        assert_eq!(state.version, STATE_VERSION);
    }

    #[test]
    fn round_trips_pending_down_and_observations() {
        let path = temp_path("roundtrip");
        let mut state = StateFile::default();
        state.mark_pending_down("PPPoE0", "restart");
        state.set_observation(StoredObservation {
            interface: "PPPoE0".to_string(),
            address: Some("100.70.12.34".to_string()),
            uptime: Some(12),
            last_change: None,
            class: Some("grey".to_string()),
            class_reason: Some("CGNAT".to_string()),
            updated_unix: 1,
        });
        state.save(&path).unwrap();

        let loaded = StateFile::load(&path).unwrap();
        assert!(loaded.is_pending_down("PPPoE0"));
        assert_eq!(loaded.observations.len(), 1);
        assert_eq!(loaded.observations[0].uptime, Some(12));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn clear_pending_down_keeps_observations() {
        let mut state = StateFile::default();
        state.mark_pending_down("PPPoE0", "restart");
        state.set_observation(StoredObservation {
            interface: "PPPoE0".to_string(),
            address: None,
            uptime: None,
            last_change: None,
            class: None,
            class_reason: None,
            updated_unix: 0,
        });
        state.clear_pending_down("PPPoE0");
        assert!(!state.is_pending_down("PPPoE0"));
        assert_eq!(state.observations.len(), 1);
    }

    #[test]
    fn corrupt_file_is_an_error_not_a_silent_reset() {
        let path = temp_path("corrupt");
        fs::write(&path, "{not json").unwrap();
        let err = StateFile::load(&path).unwrap_err();
        assert!(err.contains("cannot parse"), "{err}");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn state_file_is_private_on_unix() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = temp_path("mode");
            StateFile::default().save(&path).unwrap();
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let _ = fs::remove_file(&path);
        }
    }
}

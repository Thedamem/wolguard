//! Comment-preserving configuration edits for the CLI/TUI.
//!
//! The daemon only reads the TOML file. Writers go through this module:
//! `toml_edit` keeps comments and unrelated keys, the result is validated with
//! the daemon's parser before it is written, and the replacement is atomic.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::config::{Config, Trigger};

pub const DEFAULT_TEMPLATE: &str = "\
# wolguard configuration.
# Edit through `wolguard setup`, `wolguard hosts`, `wolguard pppoe` or the TUI.
# This file is still valid TOML and can be edited manually.

wol_enabled = true
";

#[derive(Clone, Debug, Default)]
pub struct HostEdit {
    /// Existing host name when editing; `None` when adding.
    pub original_name: Option<String>,
    pub name: String,
    pub ip: String,
    pub mac: String,
    pub enabled: bool,
    pub check_interval_secs: Option<u64>,
    pub wake_retry_secs: Option<u64>,
    pub failures_before_wake: Option<u32>,
    pub max_wakes_per_hour: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct TargetEdit {
    pub interface: String,
    pub enabled: Option<bool>,
    pub trigger: Option<Trigger>,
    pub poll_secs: Option<u64>,
    pub down_secs: Option<u64>,
    pub settle_secs: Option<u64>,
    pub retry_secs: Option<u64>,
    pub max_restarts_per_hour: Option<u32>,
    pub white_cidrs: Option<Vec<String>>,
    pub grey_cidrs: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
pub struct PppoeDefaults {
    pub trigger: Trigger,
    pub poll_secs: u64,
    pub down_secs: u64,
    pub settle_secs: u64,
    pub retry_secs: u64,
    pub max_restarts_per_hour: u32,
    pub white_cidrs: Vec<String>,
    pub grey_cidrs: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct RciEdit {
    pub url: String,
    pub token: String,
    pub timeout_ms: u64,
    pub auto_token: bool,
    pub ndmc_path: String,
}

pub struct ConfigDoc {
    path: PathBuf,
    doc: DocumentMut,
    original: String,
}

impl ConfigDoc {
    pub fn load(path: impl Into<PathBuf>) -> Result<ConfigDoc, String> {
        let path = path.into();
        let original = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => DEFAULT_TEMPLATE.to_string(),
            Err(err) => {
                return Err(format!("cannot read config {}: {err}", path.display()));
            }
        };
        let doc: DocumentMut = original
            .parse()
            .map_err(|err| format!("cannot parse config {}: {err}", path.display()))?;
        Ok(ConfigDoc {
            path,
            doc,
            original,
        })
    }

    pub fn set_wol_enabled(&mut self, enabled: bool) {
        self.set_top("wol_enabled", value(enabled));
    }

    pub fn set_top_int(&mut self, key: &str, value: u64) {
        self.set_top(key, toml_edit::value(value as i64));
    }

    pub fn set_top_bool(&mut self, key: &str, value: bool) {
        self.set_top(key, toml_edit::value(value));
    }

    pub fn set_top_string(&mut self, key: &str, value: &str) {
        self.set_top(key, toml_edit::value(value));
    }

    fn set_top(&mut self, key: &str, item: Item) {
        self.doc.as_table_mut().insert(key, item);
    }

    pub fn upsert_host(&mut self, edit: &HostEdit) -> Result<(), String> {
        let hosts = self.host_array()?;
        let lookup = edit.original_name.as_deref().unwrap_or(&edit.name);
        let index = hosts.iter().position(|table| {
            table
                .get("name")
                .and_then(Item::as_str)
                .map(|name| name == lookup)
                .unwrap_or(false)
        });
        let table = match index {
            Some(index) => hosts.get_mut(index).expect("host index is valid"),
            None => {
                hosts.push(Table::new());
                let last = hosts.len() - 1;
                hosts.get_mut(last).expect("new host exists")
            }
        };
        table.insert("name", value(edit.name.as_str()));
        table.insert("ip", value(edit.ip.as_str()));
        table.insert("mac", value(edit.mac.as_str()));
        table.insert("enabled", value(edit.enabled));
        set_optional_int(table, "check_interval_secs", edit.check_interval_secs);
        set_optional_int(table, "wake_retry_secs", edit.wake_retry_secs);
        set_optional_int(
            table,
            "failures_before_wake",
            edit.failures_before_wake.map(u64::from),
        );
        set_optional_int(
            table,
            "max_wakes_per_hour",
            edit.max_wakes_per_hour.map(u64::from),
        );
        Ok(())
    }

    pub fn remove_host(&mut self, name: &str) -> Result<(), String> {
        let hosts = self.host_array()?;
        let index = hosts
            .iter()
            .position(|table| table.get("name").and_then(Item::as_str) == Some(name))
            .ok_or_else(|| format!("host {name:?} is not configured"))?;
        hosts.remove(index);
        Ok(())
    }

    pub fn set_host_enabled(&mut self, name: &str, enabled: bool) -> Result<(), String> {
        let hosts = self.host_array()?;
        let table = hosts
            .iter_mut()
            .find(|table| table.get("name").and_then(Item::as_str) == Some(name))
            .ok_or_else(|| format!("host {name:?} is not configured"))?;
        table.insert("enabled", value(enabled));
        Ok(())
    }

    fn host_array(&mut self) -> Result<&mut ArrayOfTables, String> {
        let root = self.doc.as_table_mut();
        if !root.contains_key("host") {
            root.insert("host", Item::ArrayOfTables(ArrayOfTables::new()));
        }
        root.get_mut("host")
            .and_then(Item::as_array_of_tables_mut)
            .ok_or_else(|| "config key `host` must be an array of tables ([[host]])".to_string())
    }

    pub fn set_pppoe_enabled(&mut self, enabled: bool) -> Result<(), String> {
        let pppoe = self.pppoe_table()?;
        pppoe.insert("enabled", value(enabled));
        Ok(())
    }

    pub fn set_pppoe_defaults(&mut self, defaults: &PppoeDefaults) -> Result<(), String> {
        let pppoe = self.pppoe_table()?;
        pppoe.insert("trigger", value(defaults.trigger.label()));
        pppoe.insert("poll_secs", value(defaults.poll_secs as i64));
        pppoe.insert("down_secs", value(defaults.down_secs as i64));
        pppoe.insert("settle_secs", value(defaults.settle_secs as i64));
        pppoe.insert("retry_secs", value(defaults.retry_secs as i64));
        pppoe.insert(
            "max_restarts_per_hour",
            value(defaults.max_restarts_per_hour as i64),
        );
        set_string_array(pppoe, "white_cidrs", &defaults.white_cidrs);
        set_string_array(pppoe, "grey_cidrs", &defaults.grey_cidrs);
        Ok(())
    }

    pub fn upsert_target(&mut self, edit: &TargetEdit) -> Result<(), String> {
        self.upsert_target_inner(edit, false)
    }

    /// Replace all rule overrides for a target. `None` means "remove the
    /// override and inherit the global default".
    pub fn set_target_rules(&mut self, edit: &TargetEdit) -> Result<(), String> {
        self.upsert_target_inner(edit, true)
    }

    fn upsert_target_inner(
        &mut self,
        edit: &TargetEdit,
        clear_missing: bool,
    ) -> Result<(), String> {
        let targets = self.target_array()?;
        let index = targets.iter().position(|table| {
            table.get("interface").and_then(Item::as_str) == Some(edit.interface.as_str())
        });
        let table = match index {
            Some(index) => targets.get_mut(index).expect("target index is valid"),
            None => {
                targets.push(Table::new());
                let last = targets.len() - 1;
                let table = targets.get_mut(last).expect("new target exists");
                table.insert("interface", value(edit.interface.as_str()));
                table.insert("enabled", value(edit.enabled.unwrap_or(true)));
                table
            }
        };
        if let Some(enabled) = edit.enabled {
            table.insert("enabled", value(enabled));
        }
        if let Some(trigger) = edit.trigger {
            table.insert("trigger", value(trigger.label()));
        } else if clear_missing {
            table.remove("trigger");
        }
        if clear_missing {
            set_optional_int(table, "poll_secs", edit.poll_secs);
            set_optional_int(table, "down_secs", edit.down_secs);
            set_optional_int(table, "settle_secs", edit.settle_secs);
            set_optional_int(table, "retry_secs", edit.retry_secs);
            set_optional_int(
                table,
                "max_restarts_per_hour",
                edit.max_restarts_per_hour.map(u64::from),
            );
        } else {
            if let Some(value) = edit.poll_secs {
                table.insert("poll_secs", toml_edit::value(value as i64));
            }
            if let Some(value) = edit.down_secs {
                table.insert("down_secs", toml_edit::value(value as i64));
            }
            if let Some(value) = edit.settle_secs {
                table.insert("settle_secs", toml_edit::value(value as i64));
            }
            if let Some(value) = edit.retry_secs {
                table.insert("retry_secs", toml_edit::value(value as i64));
            }
            if let Some(value) = edit.max_restarts_per_hour {
                table.insert("max_restarts_per_hour", toml_edit::value(value as i64));
            }
        }
        match (&edit.white_cidrs, clear_missing) {
            (Some(white), _) => set_string_array(table, "white_cidrs", white),
            (None, true) => {
                table.remove("white_cidrs");
            }
            (None, false) => {}
        }
        match (&edit.grey_cidrs, clear_missing) {
            (Some(grey), _) => set_string_array(table, "grey_cidrs", grey),
            (None, true) => {
                table.remove("grey_cidrs");
            }
            (None, false) => {}
        }
        Ok(())
    }

    /// Ensure a `[[pppoe.target]]` exists and is enabled. Existing rules are
    /// never rewritten.
    pub fn watch_target(&mut self, interface: &str) -> Result<(), String> {
        self.upsert_target(&TargetEdit {
            interface: interface.to_string(),
            enabled: Some(true),
            ..TargetEdit::default()
        })
    }

    pub fn set_target_enabled(&mut self, interface: &str, enabled: bool) -> Result<(), String> {
        let targets = self.target_array()?;
        let table = targets
            .iter_mut()
            .find(|table| table.get("interface").and_then(Item::as_str) == Some(interface))
            .ok_or_else(|| format!("PPPoE target {interface:?} is not configured"))?;
        table.insert("enabled", value(enabled));
        Ok(())
    }

    pub fn remove_target(&mut self, interface: &str) -> Result<(), String> {
        let targets = self.target_array()?;
        let index = targets
            .iter()
            .position(|table| table.get("interface").and_then(Item::as_str) == Some(interface))
            .ok_or_else(|| format!("PPPoE target {interface:?} is not configured"))?;
        targets.remove(index);
        Ok(())
    }

    fn pppoe_table(&mut self) -> Result<&mut Table, String> {
        ensure_table(self.doc.as_table_mut(), "pppoe")
    }

    fn target_array(&mut self) -> Result<&mut ArrayOfTables, String> {
        let pppoe = self.pppoe_table()?;
        if !pppoe.contains_key("target") {
            pppoe.insert("target", Item::ArrayOfTables(ArrayOfTables::new()));
        }
        pppoe
            .get_mut("target")
            .and_then(Item::as_array_of_tables_mut)
            .ok_or_else(|| {
                "config key `pppoe.target` must be an array of tables ([[pppoe.target]])"
                    .to_string()
            })
    }

    pub fn set_rci(&mut self, edit: &RciEdit) -> Result<(), String> {
        let root = self.doc.as_table_mut();
        let rci = ensure_table(root, "rci")?;
        rci.insert("url", value(edit.url.as_str()));
        rci.insert("timeout_ms", value(edit.timeout_ms as i64));
        rci.insert("auto_token", value(edit.auto_token));
        rci.insert("ndmc_path", value(edit.ndmc_path.as_str()));
        if edit.token.trim().is_empty() {
            rci.remove("token");
        } else {
            rci.insert("token", value(edit.token.as_str()));
        }
        Ok(())
    }

    /// Validate and atomically replace the configuration file.
    ///
    /// `force` overwrites a file that changed on disk since `load`.
    pub fn save(&mut self, force: bool) -> Result<(), String> {
        let rendered = self.doc.to_string();
        Config::from_toml_str(&rendered)
            .map_err(|err| format!("configuration would be invalid: {err}"))?;

        if self.path.exists() {
            let current = fs::read_to_string(&self.path)
                .map_err(|err| format!("cannot re-read config {}: {err}", self.path.display()))?;
            if !force && hash_text(&current) != hash_text(&self.original) {
                return Err(format!(
                    "configuration {} changed on disk since it was loaded; reload it or force the save",
                    self.path.display()
                ));
            }
        }

        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent).map_err(|err| {
                format!("cannot create config directory {}: {err}", parent.display())
            })?;
        }

        if self.path.exists() {
            let backup = backup_path(&self.path);
            if let Err(err) = fs::copy(&self.path, &backup) {
                return Err(format!(
                    "cannot back up {} to {}: {err}",
                    self.path.display(),
                    backup.display()
                ));
            }
        }

        let mode = self.desired_mode();
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("wolguard.conf");
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let temp = self
            .path
            .with_file_name(format!(".{file_name}.tmp.{}.{nanos}", std::process::id()));
        write_with_mode(&temp, rendered.as_bytes(), mode)?;
        if let Err(err) = fs::rename(&temp, &self.path) {
            let _ = fs::remove_file(&temp);
            return Err(format!(
                "cannot replace config {}: {err}",
                self.path.display()
            ));
        }
        self.original = rendered;
        Ok(())
    }

    fn desired_mode(&self) -> u32 {
        let token_present = self
            .doc
            .as_table()
            .get("rci")
            .and_then(Item::as_table)
            .and_then(|rci| rci.get("token"))
            .and_then(Item::as_str)
            .map(|token| !token.trim().is_empty())
            .unwrap_or(false);
        if token_present {
            return 0o600;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = fs::metadata(&self.path) {
                return metadata.permissions().mode() & 0o777;
            }
        }
        0o644
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("wolguard.conf");
    path.with_file_name(format!("{file_name}.bak"))
}

fn hash_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn ensure_table<'a>(root: &'a mut Table, key: &str) -> Result<&'a mut Table, String> {
    if !root.contains_key(key) {
        root.insert(key, Item::Table(Table::new()));
    }
    root.get_mut(key)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| format!("config key `{key}` must be a table ([{key}])"))
}

fn set_optional_int(table: &mut Table, key: &str, value: Option<u64>) {
    match value {
        Some(value) => {
            table.insert(key, toml_edit::value(value as i64));
        }
        None => {
            table.remove(key);
        }
    }
}

fn set_string_array(table: &mut Table, key: &str, values: &[String]) {
    let mut array = Array::new();
    for entry in values {
        array.push(entry.as_str());
    }
    table.insert(key, Item::Value(Value::Array(array)));
}

fn write_with_mode(path: &Path, contents: &[u8], mode: u32) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
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
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_file(name: &str, contents: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "wolguard-conf-{}-{}-{name}.conf",
            std::process::id(),
            nanos
        ));
        fs::write(&path, contents).unwrap();
        path
    }

    const SAMPLE: &str = r#"
# keep this comment
check_interval_secs = 10

[[host]]
name = "server"
ip = "192.168.1.10"
mac = "AA:BB:CC:DD:EE:FF"

[pppoe]
# keep the pppoe comment
trigger = "on-change"

[[pppoe.target]]
interface = "PPPoE0"
retry_secs = 99
"#;

    #[test]
    fn edits_a_host_and_preserves_comments() {
        let path = temp_file("host", SAMPLE);
        let mut doc = ConfigDoc::load(&path).unwrap();
        doc.upsert_host(&HostEdit {
            original_name: Some("server".to_string()),
            name: "server".to_string(),
            ip: "192.168.1.20".to_string(),
            mac: "AA:BB:CC:DD:EE:FF".to_string(),
            enabled: true,
            check_interval_secs: Some(30),
            ..HostEdit::default()
        })
        .unwrap();
        doc.save(false).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep this comment"));
        assert!(text.contains("# keep the pppoe comment"));
        let config = Config::from_toml_str(&text).unwrap();
        let hosts = config.host_settings().unwrap();
        assert_eq!(hosts[0].ip.to_string(), "192.168.1.20");
        assert_eq!(hosts[0].check_interval, std::time::Duration::from_secs(30));
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(backup_path(&path));
    }

    #[test]
    fn watch_does_not_overwrite_existing_target_rules() {
        let path = temp_file("watch", SAMPLE);
        let mut doc = ConfigDoc::load(&path).unwrap();
        doc.watch_target("PPPoE0").unwrap();
        doc.save(false).unwrap();
        let config = Config::from_toml_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let targets = config.pppoe_targets().unwrap();
        assert_eq!(targets[0].retry, std::time::Duration::from_secs(99));
        assert!(targets[0].enabled);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(backup_path(&path));
    }

    #[test]
    fn watch_creates_a_new_target() {
        let path = temp_file(
            "newwatch",
            r#"
[[host]]
name = "server"
ip = "192.168.1.10"
mac = "AA:BB:CC:DD:EE:FF"
"#,
        );
        let mut doc = ConfigDoc::load(&path).unwrap();
        doc.watch_target("PPPoE1").unwrap();
        doc.save(false).unwrap();
        let config = Config::from_toml_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let targets = config.pppoe_targets().unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].interface, "PPPoE1");
        assert!(targets[0].enabled);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(backup_path(&path));
    }

    #[test]
    fn unwatch_keeps_the_target_and_its_rules() {
        let path = temp_file("unwatch", SAMPLE);
        let mut doc = ConfigDoc::load(&path).unwrap();
        doc.set_target_enabled("PPPoE0", false).unwrap();
        doc.save(false).unwrap();
        let config = Config::from_toml_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let targets = config.pppoe_targets().unwrap();
        assert_eq!(targets.len(), 1);
        assert!(!targets[0].enabled);
        assert_eq!(targets[0].retry, std::time::Duration::from_secs(99));
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(backup_path(&path));
    }

    #[test]
    fn invalid_configuration_is_not_written() {
        let path = temp_file("invalid", SAMPLE);
        let before = fs::read_to_string(&path).unwrap();
        let mut doc = ConfigDoc::load(&path).unwrap();
        doc.set_top_int("check_interval_secs", 0);
        let err = doc.save(false).unwrap_err();
        assert!(err.contains("greater than zero"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(backup_path(&path));
    }

    #[test]
    fn removing_the_last_host_without_a_target_is_rejected() {
        let path = temp_file(
            "lasthost",
            r#"
[[host]]
name = "server"
ip = "192.168.1.10"
mac = "AA:BB:CC:DD:EE:FF"
"#,
        );
        let mut doc = ConfigDoc::load(&path).unwrap();
        doc.remove_host("server").unwrap();
        let err = doc.save(false).unwrap_err();
        assert!(err.contains("nothing to monitor"), "{err}");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(backup_path(&path));
    }

    #[test]
    fn detects_a_concurrent_edit() {
        let path = temp_file("conflict", SAMPLE);
        let mut doc = ConfigDoc::load(&path).unwrap();
        fs::write(&path, format!("{}\n# external edit\n", SAMPLE)).unwrap();
        let err = doc.save(false).unwrap_err();
        assert!(err.contains("changed on disk"), "{err}");
        doc.save(true).unwrap();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(backup_path(&path));
    }

    #[test]
    fn token_sets_a_private_file_mode() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = temp_file("token", SAMPLE);
            let mut doc = ConfigDoc::load(&path).unwrap();
            doc.set_rci(&RciEdit {
                url: "http://127.0.0.1:79".to_string(),
                token: "abcdef1234567890".to_string(),
                timeout_ms: 2000,
                auto_token: true,
                ndmc_path: "ndmc".to_string(),
            })
            .unwrap();
            doc.save(false).unwrap();
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(backup_path(&path));
        }
    }

    #[test]
    fn creates_a_missing_file_from_the_template() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "wolguard-conf-{}-{}-missing.conf",
            std::process::id(),
            nanos
        ));
        assert!(!path.exists());
        let mut doc = ConfigDoc::load(&path).unwrap();
        doc.watch_target("PPPoE0").unwrap();
        doc.save(false).unwrap();
        let config = Config::from_toml_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config.pppoe_targets().unwrap().len(), 1);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(backup_path(&path));
    }
}

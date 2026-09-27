use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Per-check persistent state: survives daemon restarts so the daemon knows
/// the check is already failing and does not re-alert immediately, and knows
/// when the check last ran (to preserve the schedule across restarts).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckState {
    pub alert_active: bool,
    /// failure seen, waiting for the `flake` retry before alerting
    pub pending_flake: bool,
    /// index into the `repeat` escalation schedule
    pub repeat_index: usize,
    pub alert_count: u64,
    /// unix epoch seconds of the last alert sent
    #[serde(default)]
    pub last_alert_at: Option<i64>,
    /// unix epoch seconds of the moment the current alert started
    /// (first failure of the incident); cleared on recovery
    #[serde(default)]
    pub failing_since: Option<i64>,
    /// message the quiet hours held back (first alert or repeat); it goes out
    /// with the summary when the window ends
    #[serde(default)]
    pub deferred_alert: Option<String>,
    /// the held-back message is a repeat, not the first alert: the escalation
    /// step advances when it is finally delivered, and a held-back repeat
    /// means the incident itself was already announced
    #[serde(default)]
    pub deferred_alert_repeat: bool,
    /// restore message for an alert that went out *before* the quiet hours;
    /// a failure and its recovery both inside the window report nothing
    #[serde(default)]
    pub deferred_restored: Option<String>,
    /// unix epoch seconds of the last run start; on startup a check runs
    /// immediately only if period has already elapsed since this
    #[serde(default)]
    pub last_run_at: Option<i64>,
}

impl Default for CheckState {
    fn default() -> Self {
        CheckState {
            alert_active: false,
            pending_flake: false,
            repeat_index: 0,
            alert_count: 0,
            last_alert_at: None,
            failing_since: None,
            deferred_alert: None,
            deferred_alert_repeat: false,
            deferred_restored: None,
            last_run_at: None,
        }
    }
}

pub type States = HashMap<String, CheckState>;

pub struct StateStore {
    dir: PathBuf,
}

impl StateStore {
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        StateStore { dir }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    pub fn load_all(&self) -> Result<States> {
        let mut map = States::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(_) => return Ok(map),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let id = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            match std::fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|raw| {
                    serde_json::from_str::<CheckState>(&raw).map_err(anyhow::Error::from)
                }) {
                Ok(state) => {
                    map.insert(id, state);
                }
                Err(e) => {
                    tracing::warn!("cannot load state {}: {e}", path.display());
                }
            }
        }
        Ok(map)
    }

    pub fn save(&self, id: &str, state: &CheckState) {
        let path = self.path(id);
        let tmp = self.dir.join(format!("{id}.json.tmp"));
        let write = || -> Result<()> {
            std::fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
            std::fs::rename(&tmp, &path)?;
            Ok(())
        };
        if let Err(e) = write() {
            tracing::warn!("cannot save state for {id}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// state files written before the quiet-hours fields existed must still
    /// load: these are exactly the files the running daemon already has on disk
    #[test]
    fn loads_state_files_without_the_quiet_hours_fields() {
        let root = std::env::temp_dir().join(format!("insomnia-compat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = StateStore::new(root.clone());
        std::fs::write(
            root.join("old.check.sh.json"),
            r#"{"alert_active":true,"pending_flake":false,"repeat_index":0,"alert_count":1,
                "last_alert_at":100,"failing_since":50,"last_run_at":200}"#,
        )
        .unwrap();

        let states = store.load_all().unwrap();
        let state = states.get("old.check.sh").expect("old state file loads");
        assert!(state.alert_active);
        assert!(state.failing_since.is_some());
        assert!(state.deferred_alert.is_none());
        assert!(!state.deferred_alert_repeat);
        assert!(state.deferred_restored.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}

//! State that belongs to the user rather than to one repository: recently
//! opened repositories, the latest plan usage reported by Claude Code, and the
//! threshold above which no new agent runs are started. Stored in
//! `~/.config/claudechain/state.json`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store::write_atomic;

const MAX_RECENT: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Recent {
    pub repo: PathBuf,
    pub data: PathBuf,
    /// Main branch chosen for the repository; empty for entries saved
    /// before it could be chosen.
    #[serde(default)]
    pub main: String,
}

/// The `rate_limit_info` of the CLI's latest `rate_limit_event`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limits {
    pub at: DateTime<Utc>,
    pub info: Value,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    recent: Vec<Recent>,
    #[serde(default)]
    limits: Option<Limits>,
    /// Percentage of any plan window at which new runs are held back.
    #[serde(default)]
    pause_above: Option<f64>,
}

pub struct Account {
    saved: Mutex<Saved>,
}

fn state_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("claudechain").join("state.json"))
}

pub fn window_name(key: &str) -> String {
    match key {
        "five_hour" => "5-hour".into(),
        "seven_day" => "weekly".into(),
        other => other.replace('_', " "),
    }
}

impl Account {
    pub fn load() -> Arc<Self> {
        let saved = state_file()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Arc::new(Self { saved: Mutex::new(saved) })
    }

    fn save(saved: &Saved) {
        let Some(path) = state_file() else { return };
        let json = serde_json::to_vec_pretty(saved).unwrap_or_default();
        if let Err(e) = write_atomic(&path, &json) {
            tracing::warn!("saving {}: {e:#}", path.display());
        }
    }

    pub fn recent(&self) -> Vec<Recent> {
        self.saved.lock().unwrap().recent.clone()
    }

    pub fn remember(&self, entry: Recent) {
        let mut saved = self.saved.lock().unwrap();
        saved.recent.retain(|r| r.repo != entry.repo);
        saved.recent.insert(0, entry);
        saved.recent.truncate(MAX_RECENT);
        Self::save(&saved);
    }

    pub fn limits(&self) -> Option<Limits> {
        self.saved.lock().unwrap().limits.clone()
    }

    /// Records a `rate_limit_info`; returns true when it differs from the last one.
    pub fn record_limits(&self, info: &Value) -> bool {
        let mut saved = self.saved.lock().unwrap();
        let changed = saved.limits.as_ref().is_none_or(|l| &l.info != info);
        saved.limits = Some(Limits { at: Utc::now(), info: info.clone() });
        if changed {
            Self::save(&saved);
        }
        changed
    }

    /// Forgets the recorded usage, e.g. after the user reset their limit
    /// outside ClaudeChain. The next agent run reports fresh values.
    pub fn forget_limits(&self) {
        let mut saved = self.saved.lock().unwrap();
        saved.limits = None;
        Self::save(&saved);
    }

    pub fn pause_above(&self) -> Option<f64> {
        self.saved.lock().unwrap().pause_above
    }

    pub fn set_pause_above(&self, value: Option<f64>) {
        let mut saved = self.saved.lock().unwrap();
        saved.pause_above = value.filter(|v| *v > 0.0);
        Self::save(&saved);
    }

    /// Why new runs should wait, if a plan window is exhausted or above the
    /// pause threshold. Windows whose reset time has passed no longer count.
    pub fn paused_reason(&self) -> Option<String> {
        let saved = self.saved.lock().unwrap();
        let info = &saved.limits.as_ref()?.info;
        let now = Utc::now().timestamp();
        let windows = info["unifiedWindows"].as_object()?;
        for (key, w) in windows {
            let resets = w["resetsAt"].as_i64().unwrap_or(0);
            if resets <= now {
                continue;
            }
            let pct = w["utilization"].as_f64().unwrap_or(0.0) * 100.0;
            if w["status"].as_str() == Some("rejected") {
                return Some(format!("{} limit reached", window_name(key)));
            }
            if let Some(limit) = saved.pause_above
                && pct >= limit
            {
                return Some(format!("{} limit at {pct:.0}% (pausing at {limit:.0}%)", window_name(key)));
            }
        }
        None
    }
}

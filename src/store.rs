use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Writes via a temp file + rename so readers never see a half-written file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp~");
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    AwaitingApproval,
    Running,
    NeedsHuman,
    Done,
}

/// What an agent writes to its run's `transition.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    pub to: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
}

/// Token usage of one run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Size of the context at the latest API call.
    #[serde(default)]
    pub context: u64,
    #[serde(default)]
    pub context_window: Option<u64>,
}

impl Tokens {
    fn from_usage(u: &serde_json::Value) -> Self {
        let n = |k: &str| u.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
        Tokens {
            input: n("input_tokens"),
            output: n("output_tokens"),
            cache_read: n("cache_read_input_tokens"),
            cache_write: n("cache_creation_input_tokens"),
            context: 0,
            context_window: None,
        }
    }
}

/// Adds up token usage from a run's stream-json events. A reply with several
/// content blocks arrives as several events with the same message id and the
/// same usage, so usage is counted once per message id.
#[derive(Debug, Default)]
pub struct TokenMeter {
    per_message: std::collections::HashMap<String, Tokens>,
    order: Vec<String>,
    final_total: Option<Tokens>,
}

impl TokenMeter {
    /// Feeds one event; returns true when the totals changed.
    pub fn observe(&mut self, event: &serde_json::Value) -> bool {
        match event["type"].as_str() {
            Some("assistant") => {
                let msg = &event["message"];
                let (Some(id), Some(usage)) = (msg["id"].as_str(), msg.get("usage")) else { return false };
                let t = Tokens::from_usage(usage);
                if self.per_message.get(id) == Some(&t) {
                    return false;
                }
                if !self.per_message.contains_key(id) {
                    self.order.push(id.to_string());
                }
                self.per_message.insert(id.to_string(), t);
                true
            }
            Some("result") => {
                let mut t = Tokens::from_usage(&event["usage"]);
                let window = event["modelUsage"]
                    .as_object()
                    .and_then(|m| m.values().filter_map(|v| v["contextWindow"].as_u64()).max());
                t.context_window = window;
                t.context = self.live().context;
                self.final_total = Some(t);
                true
            }
            _ => false,
        }
    }

    /// Totals so far; the result event's exact totals once the run is done.
    pub fn total(&self) -> Tokens {
        self.final_total.clone().unwrap_or_else(|| self.live())
    }

    fn live(&self) -> Tokens {
        let mut sum = Tokens::default();
        for t in self.per_message.values() {
            sum.input += t.input;
            sum.output += t.output;
            sum.cache_read += t.cache_read;
            sum.cache_write += t.cache_write;
        }
        if let Some(last) = self.order.last().and_then(|id| self.per_message.get(id)) {
            sum.context = last.input + last.cache_read + last.cache_write;
        }
        sum
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub at: DateTime<Utc>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingResume {
    pub from_run: u32,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub n: u32,
    pub step: String,
    pub model: String,
    pub started: DateTime<Utc>,
    #[serde(default)]
    pub ended: Option<DateTime<Utc>>,
    /// Set when this run continued an earlier session.
    #[serde(default)]
    pub resumed_from: Option<u32>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub tokens: Option<Tokens>,
    #[serde(default)]
    pub transition: Option<Transition>,
    /// Human-readable result: where the item went, or why it stopped.
    #[serde(default)]
    pub outcome: Option<String>,
    /// True when the run ended without a valid transition.
    #[serde(default)]
    pub failed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    #[serde(default)]
    pub number: Option<u64>,
    pub title: String,
    #[serde(default)]
    pub url: Option<String>,
    pub branch: String,
    pub created: DateTime<Utc>,
    /// The step the item is in (or was last in, when done).
    pub step: String,
    pub status: Status,
    /// Why the item needs a human, or other status detail.
    #[serde(default)]
    pub status_detail: Option<String>,
    /// When the item entered its current state; used for queue order.
    pub since: DateTime<Utc>,
    /// Number of backward moves so far.
    #[serde(default)]
    pub iteration: u32,
    /// Approval granted for the current step.
    #[serde(default)]
    pub approved: bool,
    #[serde(default)]
    pub runs: Vec<Run>,
    #[serde(default)]
    pub pending_notes: Vec<Note>,
    #[serde(default)]
    pub pending_resume: Option<PendingResume>,
    /// Commit the worktree was created from; used for diffs.
    #[serde(default)]
    pub base_commit: Option<String>,
    #[serde(default)]
    pub pr_url: Option<String>,
}

impl Item {
    pub fn next_run_number(&self) -> u32 {
        self.runs.last().map_or(1, |r| r.n + 1)
    }

    pub fn run(&self, n: u32) -> Option<&Run> {
        self.runs.iter().find(|r| r.n == n)
    }

    pub fn run_mut(&mut self, n: u32) -> Option<&mut Run> {
        self.runs.iter_mut().find(|r| r.n == n)
    }

    /// Moves the item into `step`, queued, resetting per-step state.
    pub fn enter_step(&mut self, step: &str) {
        self.step = step.to_string();
        self.status = Status::Queued;
        self.status_detail = None;
        self.approved = false;
        self.since = Utc::now();
    }

    pub fn set_status(&mut self, status: Status, detail: Option<String>) {
        self.status = status;
        self.status_detail = detail;
        self.since = Utc::now();
    }
}

/// Paths of everything on disk. All state lives under the data directory.
#[derive(Debug, Clone)]
pub struct Layout {
    pub repo: PathBuf,
    pub data: PathBuf,
}

impl Layout {
    pub fn items_dir(&self) -> PathBuf {
        self.data.join("items")
    }
    pub fn item_dir(&self, id: &str) -> PathBuf {
        self.items_dir().join(id)
    }
    pub fn item_json(&self, id: &str) -> PathBuf {
        self.item_dir(id).join("item.json")
    }
    pub fn issue_md(&self, id: &str) -> PathBuf {
        self.item_dir(id).join("issue.md")
    }
    pub fn worktree(&self, id: &str) -> PathBuf {
        self.data.join("worktrees").join(id)
    }
    pub fn run_name(n: u32, step: &str) -> String {
        format!("{n:03}-{step}")
    }
    pub fn run_dir(&self, id: &str, n: u32, step: &str) -> PathBuf {
        self.item_dir(id).join("runs").join(Self::run_name(n, step))
    }
    pub fn message_file(&self, id: &str, n: u32, step: &str) -> PathBuf {
        self.item_dir(id).join("messages").join(format!("{}.md", Self::run_name(n, step)))
    }
    pub fn transition_file(&self, id: &str, n: u32, step: &str) -> PathBuf {
        self.run_dir(id, n, step).join("transition.json")
    }
}

pub fn save_item(layout: &Layout, item: &Item) -> Result<()> {
    let json = serde_json::to_vec_pretty(item)?;
    write_atomic(&layout.item_json(&item.id), &json)
}

pub fn load_items(layout: &Layout) -> Result<BTreeMap<String, Item>> {
    let mut items = BTreeMap::new();
    let dir = layout.items_dir();
    if !dir.exists() {
        return Ok(items);
    }
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path().join("item.json");
        if !path.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&path)?;
        match serde_json::from_str::<Item>(&text) {
            Ok(item) => {
                items.insert(item.id.clone(), item);
            }
            Err(e) => tracing::warn!("skipping {}: {e}", path.display()),
        }
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant(id: &str, input: u64, cache_read: u64, output: u64) -> serde_json::Value {
        json!({"type": "assistant", "message": {"id": id, "usage": {
            "input_tokens": input, "cache_read_input_tokens": cache_read,
            "cache_creation_input_tokens": 10, "output_tokens": output}}})
    }

    #[test]
    fn meter_counts_each_message_once_and_prefers_the_result() {
        let mut m = TokenMeter::default();
        assert!(m.observe(&assistant("a", 5, 100, 20)));
        // Same message again (another content block): no change.
        assert!(!m.observe(&assistant("a", 5, 100, 20)));
        assert!(m.observe(&assistant("b", 3, 200, 7)));
        let t = m.total();
        assert_eq!((t.input, t.output, t.cache_read, t.cache_write), (8, 27, 300, 20));
        assert_eq!(t.context, 3 + 200 + 10);

        m.observe(&json!({"type": "result", "usage": {"input_tokens": 9, "output_tokens": 30,
            "cache_read_input_tokens": 310, "cache_creation_input_tokens": 25},
            "modelUsage": {"claude-haiku-5-5": {"contextWindow": 1000000}}}));
        let t = m.total();
        assert_eq!((t.input, t.output, t.cache_read, t.cache_write), (9, 30, 310, 25));
        assert_eq!((t.context, t.context_window), (213, Some(1_000_000)));
    }
}

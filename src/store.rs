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

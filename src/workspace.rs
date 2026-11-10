//! Which repository is open. The server can start without one and the user
//! picks (or switches) it from the UI. Recent choices are remembered in
//! `~/.config/claudechain/state.json`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::git;
use crate::orchestrator::{App, UiEvent};
use crate::store::{Layout, write_atomic};

const MAX_RECENT: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Recent {
    pub repo: PathBuf,
    pub data: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    recent: Vec<Recent>,
}

pub struct Workspace {
    claude: String,
    pub events: broadcast::Sender<UiEvent>,
    current: RwLock<Option<Arc<App>>>,
    /// Serializes open/switch so two requests can't race.
    switching: tokio::sync::Mutex<()>,
    saved: Mutex<Saved>,
}

fn state_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("claudechain").join("state.json"))
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn default_data_dir(repo: &Path) -> PathBuf {
    repo.join(".claudechain")
}

impl Workspace {
    pub fn new(claude: String) -> Arc<Self> {
        let saved = state_file()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let (events, _) = broadcast::channel(4096);
        Arc::new(Self {
            claude,
            events,
            current: RwLock::new(None),
            switching: tokio::sync::Mutex::new(()),
            saved: Mutex::new(saved),
        })
    }

    pub fn app(&self) -> Option<Arc<App>> {
        self.current.read().unwrap().clone()
    }

    pub fn recent(&self) -> Vec<Recent> {
        self.saved.lock().unwrap().recent.clone()
    }

    /// Opens `repo` (with its data directory), replacing the current one.
    pub async fn open(&self, repo: &Path, data: Option<&Path>) -> Result<()> {
        let _guard = self.switching.lock().await;
        let repo = std::fs::canonicalize(repo).with_context(|| format!("{} does not exist", repo.display()))?;
        git::check_repo(&repo)
            .await
            .map_err(|_| anyhow!("{} is not a git repository", repo.display()))?;
        let data = match data {
            Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
            _ => default_data_dir(&repo),
        };
        std::fs::create_dir_all(&data).with_context(|| format!("creating {}", data.display()))?;
        let data = std::fs::canonicalize(&data)?;

        if let Some(cur) = self.app()
            && cur.layout.repo == repo && cur.layout.data == data {
                return Ok(());
            }
        git::exclude_path(&repo, &data).await?;
        let app = App::new(Layout { repo: repo.clone(), data: data.clone() }, self.claude.clone(), self.events.clone())?;

        if let Some(old) = self.app() {
            old.close()?;
        }
        tokio::spawn(app.clone().scheduler());
        *self.current.write().unwrap() = Some(app);
        tracing::info!("opened repo {} (data {})", repo.display(), data.display());
        self.remember(Recent { repo, data });
        let _ = self.events.send(UiEvent::Workspace);
        Ok(())
    }

    fn remember(&self, entry: Recent) {
        let mut saved = self.saved.lock().unwrap();
        saved.recent.retain(|r| r.repo != entry.repo);
        saved.recent.insert(0, entry);
        saved.recent.truncate(MAX_RECENT);
        if let Some(path) = state_file() {
            let json = serde_json::to_vec_pretty(&*saved).unwrap_or_default();
            if let Err(e) = write_atomic(&path, &json) {
                tracing::warn!("saving {}: {e:#}", path.display());
            }
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DirEntry {
    pub name: String,
    pub path: PathBuf,
    /// Contains a `.git` entry (repository or worktree root).
    pub git: bool,
}

#[derive(Debug, Serialize)]
pub struct DirListing {
    pub path: PathBuf,
    pub parent: Option<PathBuf>,
    pub git: bool,
    pub entries: Vec<DirEntry>,
}

/// Lists the subdirectories of `path` (home when empty), for the folder picker.
pub fn list_dir(path: &str, show_hidden: bool) -> Result<DirListing> {
    let path = if path.trim().is_empty() { home_dir() } else { PathBuf::from(path.trim()) };
    let path = std::fs::canonicalize(&path).with_context(|| format!("{} does not exist", path.display()))?;
    let mut entries = vec![];
    for entry in std::fs::read_dir(&path).with_context(|| format!("reading {}", path.display()))? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().to_string();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        entries.push(DirEntry { git: p.join(".git").exists(), name, path: p });
    }
    entries.sort_by_key(|e| e.name.to_lowercase());
    entries.truncate(1000);
    Ok(DirListing {
        git: path.join(".git").exists(),
        parent: path.parent().map(Path::to_path_buf),
        path,
        entries,
    })
}

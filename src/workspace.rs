//! Which repository is open. The server can start without one and the user
//! picks (or switches) it from the UI. Recent choices are remembered by
//! `Account`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::account::{Account, Recent};
use crate::git;
use crate::orchestrator::{App, UiEvent};
use crate::store::Layout;
use crate::sysmon::Monitor;

pub struct Workspace {
    claude: String,
    pub events: broadcast::Sender<UiEvent>,
    current: RwLock<Option<Arc<App>>>,
    /// Serializes open/switch so two requests can't race.
    switching: tokio::sync::Mutex<()>,
    pub account: Arc<Account>,
    pub monitor: Monitor,
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn default_data_dir(repo: &Path) -> PathBuf {
    repo.join(".claudechain")
}

impl Workspace {
    pub fn new(claude: String) -> Arc<Self> {
        let (events, _) = broadcast::channel(4096);
        Arc::new(Self {
            claude,
            events,
            current: RwLock::new(None),
            switching: tokio::sync::Mutex::new(()),
            account: Account::load(),
            monitor: Monitor::new(),
        })
    }

    /// Samples system resources forever (for the Resources page).
    pub async fn monitor_loop(self: Arc<Self>) {
        let mut last_disk: Option<std::time::Instant> = None;
        loop {
            let disk = self.app().map(|a| a.layout.data.clone());
            let ws = self.clone();
            let _ = tokio::task::spawn_blocking(move || ws.monitor.sample(disk.as_deref())).await;
            if let Some(app) = self.app() {
                app.record_usage(&self.monitor);
                if last_disk.is_none_or(|t| t.elapsed() >= crate::sysmon::DISK_INTERVAL) {
                    last_disk = Some(std::time::Instant::now());
                    let dirs = app.item_dirs();
                    let sizes = tokio::task::spawn_blocking(move || {
                        dirs.into_iter()
                            .map(|(id, paths)| (id, paths.iter().map(|p| crate::sysmon::dir_size(p)).sum()))
                            .collect::<Vec<(String, u64)>>()
                    })
                    .await;
                    if let Ok(sizes) = sizes {
                        app.record_disk(sizes);
                    }
                }
            }
            tokio::time::sleep(crate::sysmon::INTERVAL).await;
        }
    }

    pub fn app(&self) -> Option<Arc<App>> {
        self.current.read().unwrap().clone()
    }

    pub fn recent(&self) -> Vec<Recent> {
        self.account.recent()
    }

    /// Opens `repo` (with its data directory and main branch), replacing the
    /// current one. Without `main`, the branch remembered for this repository
    /// is used, else the one git suggests.
    pub async fn open(&self, repo: &Path, data: Option<&Path>, main: Option<&str>) -> Result<()> {
        let _guard = self.switching.lock().await;
        let repo = std::fs::canonicalize(repo).with_context(|| format!("{} does not exist", repo.display()))?;
        git::check_repo(&repo)
            .await
            .map_err(|_| anyhow!("{} is not a git repository", repo.display()))?;
        let main = match main.map(str::trim).filter(|m| !m.is_empty()) {
            Some(m) => m.to_string(),
            None => match self.recent().into_iter().find(|r| r.repo == repo && !r.main.is_empty()) {
                Some(r) => r.main,
                None => git::default_branch(&repo).await?,
            },
        };
        if !git::branch_exists(&repo, &main).await? {
            bail!("branch '{main}' does not exist in {}", repo.display());
        }
        let data = match data {
            Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
            _ => default_data_dir(&repo),
        };
        std::fs::create_dir_all(&data).with_context(|| format!("creating {}", data.display()))?;
        let data = std::fs::canonicalize(&data)?;

        if let Some(cur) = self.app()
            && cur.layout.repo == repo && cur.layout.data == data && cur.layout.main == main {
                return Ok(());
            }
        git::exclude_path(&repo, &data).await?;
        let layout = Layout { repo: repo.clone(), data: data.clone(), main: main.clone() };
        let app = App::new(layout, self.claude.clone(), self.events.clone(), self.account.clone())?;

        if let Some(old) = self.app() {
            old.close()?;
        }
        tokio::spawn(app.clone().scheduler());
        *self.current.write().unwrap() = Some(app);
        tracing::info!("opened repo {} (data {}, main branch {main})", repo.display(), data.display());
        self.account.remember(Recent { repo, data, main });
        let _ = self.events.send(UiEvent::Workspace);
        Ok(())
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

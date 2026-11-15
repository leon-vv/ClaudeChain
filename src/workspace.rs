//! Which repositories are open. The server can start without one and the
//! user opens them from the UI. Several can be open at once: each browser tab
//! looks at one of them (chosen by its id in the page URL), and every open
//! repository keeps scheduling agents whether or not a tab shows it. Recent
//! choices are remembered by `Account`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::account::{Account, Recent};
use crate::git;
use crate::orchestrator::{App, Envelope, UiEvent};
use crate::store::Layout;
use crate::sysmon::Monitor;

pub struct Workspace {
    claude: String,
    pub events: broadcast::Sender<Envelope>,
    /// Open repositories by id.
    apps: RwLock<BTreeMap<String, Arc<App>>>,
    /// The most recently opened repository, for tabs that haven't picked one.
    default: RwLock<Option<String>>,
    /// Serializes open/switch so two requests can't race.
    switching: tokio::sync::Mutex<()>,
    pub account: Arc<Account>,
    pub monitor: Monitor,
}

/// An open repository, as listed to the UI.
#[derive(Debug, Serialize)]
pub struct OpenRepo {
    pub id: String,
    pub repo: PathBuf,
    pub data: PathBuf,
    pub main: String,
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn default_data_dir(repo: &Path) -> PathBuf {
    repo.join(".claudechain")
}

/// Stable id for a repository and its data directory (both canonical). It
/// goes in page URLs, so it stays the same across restarts (FNV-1a).
pub fn repo_id(repo: &Path, data: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let repo = repo.as_os_str().as_encoded_bytes();
    let data = data.as_os_str().as_encoded_bytes();
    for b in repo.iter().chain(&[0u8]).chain(data) {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

impl Workspace {
    pub fn new(claude: String) -> Arc<Self> {
        let (events, _) = broadcast::channel(4096);
        Arc::new(Self {
            claude,
            events,
            apps: RwLock::new(BTreeMap::new()),
            default: RwLock::new(None),
            switching: tokio::sync::Mutex::new(()),
            account: Account::load(),
            monitor: Monitor::new(),
        })
    }

    /// Samples system resources forever (for the Resources page).
    pub async fn monitor_loop(self: Arc<Self>) {
        let mut last_disk: Option<std::time::Instant> = None;
        loop {
            let apps = self.apps();
            let disk = self.app(None).or_else(|| apps.first().cloned()).map(|a| a.layout.data.clone());
            let ws = self.clone();
            let _ = tokio::task::spawn_blocking(move || ws.monitor.sample(disk.as_deref())).await;
            let disk_due = last_disk.is_none_or(|t| t.elapsed() >= crate::sysmon::DISK_INTERVAL);
            if disk_due && !apps.is_empty() {
                last_disk = Some(std::time::Instant::now());
            }
            for app in apps {
                app.record_usage(&self.monitor);
                if disk_due {
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

    /// The open repository with this id, or without an id the default one
    /// (the most recently opened).
    pub fn app(&self, id: Option<&str>) -> Option<Arc<App>> {
        let default = self.default.read().unwrap().clone();
        let id = id.map(str::to_string).or(default)?;
        self.apps.read().unwrap().get(&id).cloned()
    }

    /// Like `app`, but a recent repository that isn't open yet (e.g. a tab
    /// left open across a server restart) is opened again.
    pub async fn resolve(&self, id: Option<&str>) -> Result<Option<Arc<App>>> {
        if let Some(app) = self.app(id) {
            return Ok(Some(app));
        }
        let Some(id) = id else { return Ok(None) };
        let Some(r) = self.recent().into_iter().find(|r| repo_id(&r.repo, &r.data) == id) else {
            return Ok(None);
        };
        let main = Some(r.main.as_str()).filter(|m| !m.is_empty());
        self.open(&r.repo, Some(&r.data), main).await?;
        Ok(self.app(Some(id)))
    }

    pub fn apps(&self) -> Vec<Arc<App>> {
        self.apps.read().unwrap().values().cloned().collect()
    }

    pub fn open_repos(&self) -> Vec<OpenRepo> {
        self.apps()
            .into_iter()
            .map(|a| OpenRepo {
                id: a.id.clone(),
                repo: a.layout.repo.clone(),
                data: a.layout.data.clone(),
                main: a.layout.main.clone(),
            })
            .collect()
    }

    pub fn recent(&self) -> Vec<Recent> {
        self.account.recent()
    }

    /// Opens `repo` (with its data directory and main branch) next to the
    /// ones already open and returns its id. Opening one that is already open
    /// does nothing, except with another main branch, which replaces it.
    /// Without `main`, the branch remembered for this repository is used,
    /// else the one git suggests.
    pub async fn open(&self, repo: &Path, data: Option<&Path>, main: Option<&str>) -> Result<String> {
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
        let id = repo_id(&repo, &data);

        let existing = self.app(Some(&id));
        if let Some(cur) = &existing
            && cur.layout.main == main {
                *self.default.write().unwrap() = Some(id.clone());
                self.account.remember(Recent { repo, data, main });
                let _ = self.events.send(Envelope::global(UiEvent::Workspace));
                return Ok(id);
            }
        if let Some(other) = self.apps().into_iter().find(|a| a.layout.data == data && a.id != id) {
            bail!("data directory {} is already used by {}", data.display(), other.layout.repo.display());
        }
        git::exclude_path(&repo, &data).await?;
        let layout = Layout { repo: repo.clone(), data: data.clone(), main: main.clone() };
        // Another main branch for an open repository: replace its app.
        if let Some(old) = &existing {
            old.close()?;
        }
        let app = App::new(id.clone(), layout, self.claude.clone(), self.events.clone(), self.account.clone())?;
        tokio::spawn(app.clone().scheduler());
        self.apps.write().unwrap().insert(id.clone(), app);
        *self.default.write().unwrap() = Some(id.clone());
        tracing::info!("opened repo {} (data {}, main branch {main}, id {id})", repo.display(), data.display());
        self.account.remember(Recent { repo, data, main });
        let _ = self.events.send(Envelope::global(UiEvent::Workspace));
        Ok(id)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_id_is_stable_and_distinct() {
        let a = repo_id(Path::new("/r/a"), Path::new("/r/a/.claudechain"));
        assert_eq!(a, repo_id(Path::new("/r/a"), Path::new("/r/a/.claudechain")));
        assert_eq!(a.len(), 16);
        assert_ne!(a, repo_id(Path::new("/r/b"), Path::new("/r/a/.claudechain")));
        assert_ne!(a, repo_id(Path::new("/r/a"), Path::new("/r/a/.other")));
        // The separator keeps the boundary between the two paths.
        assert_ne!(repo_id(Path::new("/ab"), Path::new("c")), repo_id(Path::new("/a"), Path::new("bc")));
    }

    #[test]
    fn envelope_filtering() {
        let global = Envelope::global(UiEvent::Workspace);
        assert!(global.is_for(Some("x")));
        assert!(global.is_for(None));
        let scoped = Envelope { repo: Some("x".into()), event: UiEvent::Pipeline };
        assert!(scoped.is_for(Some("x")));
        assert!(!scoped.is_for(Some("y")));
        assert!(!scoped.is_for(None));
    }
}

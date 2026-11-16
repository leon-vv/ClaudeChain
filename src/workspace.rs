//! Which repository and pipeline are open. The server can start without one
//! and the user picks (or switches) it from the UI. Recent choices are
//! remembered by `Account`.
//!
//! Every pipeline of the open repository runs at the same time, each in its own
//! `App` with its own scheduler; `current` is just the one the UI shows, so
//! switching pipelines never interrupts agents. Switching to another repository
//! closes the idle pipelines of the old one; busy ones keep running until the
//! next switch finds them idle.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::account::{Account, Recent};
use crate::git;
use crate::orchestrator::{App, Tagged, UiEvent};
use crate::store::Layout;
use crate::sysmon::Monitor;

pub struct Workspace {
    claude: String,
    pub events: broadcast::Sender<Tagged>,
    /// The pipeline shown in the UI (one of `apps`).
    current: RwLock<Option<Arc<App>>>,
    /// Every live pipeline, the current one included.
    apps: RwLock<Vec<Arc<App>>>,
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

/// File in each pipeline's data directory naming that pipeline and listing all
/// pipelines (data directories) of the repository, so the UI can offer them.
pub const PIPELINES_FILE: &str = "pipelines.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineRef {
    pub name: String,
    pub data: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Pipelines {
    /// Name of the pipeline whose data directory holds this file.
    pub name: String,
    /// All known pipelines of the repository, this one included.
    #[serde(default)]
    pub pipelines: Vec<PipelineRef>,
}

fn read_pipelines(data: &Path) -> Option<Pipelines> {
    let text = std::fs::read_to_string(data.join(PIPELINES_FILE)).ok()?;
    match serde_json::from_str(&text) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!("ignoring invalid {}: {e}", data.join(PIPELINES_FILE).display());
            None
        }
    }
}

fn write_pipelines(data: &Path, p: &Pipelines) -> Result<()> {
    let path = data.join(PIPELINES_FILE);
    let tmp = data.join(format!("{PIPELINES_FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_string_pretty(p)? + "\n").with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// The pipelines file of `data`, with defaults when it is missing: the name is
/// "Default" for `<repo>/.claudechain` and the folder name otherwise, and the
/// list always contains `data` itself.
pub fn pipelines(repo: &Path, data: &Path) -> Pipelines {
    let mut p = read_pipelines(data).unwrap_or_default();
    if p.name.trim().is_empty() {
        p.name = p
            .pipelines
            .iter()
            .find(|r| r.data == data)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| default_pipeline_name(repo, data));
    }
    match p.pipelines.iter_mut().find(|r| r.data == data) {
        Some(r) => r.name = p.name.clone(),
        None => p.pipelines.insert(0, PipelineRef { name: p.name.clone(), data: data.to_path_buf() }),
    }
    p
}

fn default_pipeline_name(repo: &Path, data: &Path) -> String {
    if data == default_data_dir(repo) {
        return "Default".into();
    }
    data.file_name().map(|n| n.to_string_lossy().trim_start_matches('.').to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "Pipeline".into())
}

/// Writes `list` to the pipelines file of every listed data directory that
/// exists, each with its own name.
fn sync_pipelines(list: &[PipelineRef]) -> Result<()> {
    for r in list {
        if r.data.is_dir() {
            write_pipelines(&r.data, &Pipelines { name: r.name.clone(), pipelines: list.to_vec() })?;
        }
    }
    Ok(())
}

fn check_name<'a>(name: &'a str, list: &[PipelineRef], except: Option<&Path>) -> Result<&'a str> {
    let name = name.trim();
    if name.is_empty() {
        bail!("the pipeline name is empty");
    }
    if list.iter().any(|r| Some(r.data.as_path()) != except && r.name.eq_ignore_ascii_case(name)) {
        bail!("a pipeline named \"{name}\" already exists");
    }
    Ok(name)
}

impl Workspace {
    pub fn new(claude: String) -> Arc<Self> {
        let (events, _) = broadcast::channel(4096);
        Arc::new(Self {
            claude,
            events,
            current: RwLock::new(None),
            apps: RwLock::new(vec![]),
            switching: tokio::sync::Mutex::new(()),
            account: Account::load(),
            monitor: Monitor::new(),
        })
    }

    /// Samples system resources forever (for the Resources page).
    pub async fn monitor_loop(self: Arc<Self>) {
        loop {
            let disk = self.app().map(|a| a.layout.data.clone());
            let ws = self.clone();
            let _ = tokio::task::spawn_blocking(move || ws.monitor.sample(disk.as_deref())).await;
            for app in self.apps() {
                app.record_usage(&self.monitor);
            }
            tokio::time::sleep(crate::sysmon::INTERVAL).await;
        }
    }

    pub fn app(&self) -> Option<Arc<App>> {
        self.current.read().unwrap().clone()
    }

    /// All live pipelines (of the open repository, plus busy ones of earlier ones).
    pub fn apps(&self) -> Vec<Arc<App>> {
        self.apps.read().unwrap().clone()
    }

    /// Sends an event that belongs to no particular pipeline.
    pub fn emit(&self, ev: UiEvent) {
        let _ = self.events.send(ev.into());
    }

    fn live(&self, data: &Path) -> Option<Arc<App>> {
        self.apps.read().unwrap().iter().find(|a| a.layout.data == data).cloned()
    }

    /// Returns the live app for `data` (canonical paths), starting it if needed.
    async fn start(&self, repo: &Path, data: &Path) -> Result<Arc<App>> {
        if let Some(app) = self.live(data) {
            if app.layout.repo != repo {
                bail!("{} is the data directory of {}, which still has running agents", data.display(), app.layout.repo.display());
            }
            return Ok(app);
        }
        git::exclude_path(repo, data).await?;
        // Make sure the folder names itself and lists the pipelines it knows.
        let list = pipelines(repo, data);
        if read_pipelines(data).is_none_or(|p| p.name != list.name || p.pipelines.len() != list.pipelines.len()) {
            write_pipelines(data, &list)?;
        }
        let app = App::new(Layout { repo: repo.to_path_buf(), data: data.to_path_buf() }, self.claude.clone(), self.events.clone(), self.account.clone())?;
        tokio::spawn(app.clone().scheduler());
        self.apps.write().unwrap().push(app.clone());
        tracing::info!("started pipeline {} of {}", data.display(), repo.display());
        Ok(app)
    }

    pub fn recent(&self) -> Vec<Recent> {
        self.account.recent()
    }

    /// Shows `repo` with the pipeline in `data` (default `<repo>/.claudechain`),
    /// and starts the other pipelines of the repository that aren't running yet.
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
        let app = self.start(&repo, &data).await?;
        *self.current.write().unwrap() = Some(app);
        tracing::info!("showing repo {} (data {})", repo.display(), data.display());

        // Pipelines of other repositories: close the idle ones, busy ones keep
        // running in the background (and are closed by a later switch).
        self.apps.write().unwrap().retain(|a| a.layout.repo == repo || a.close().is_err());
        // The other pipelines of this repository run too, so their queued and
        // requeued items progress while another one is shown.
        for r in pipelines(&repo, &data).pipelines {
            if r.data != data && r.data.is_dir() {
                let Ok(other) = std::fs::canonicalize(&r.data) else { continue };
                if let Err(e) = self.start(&repo, &other).await {
                    tracing::warn!("could not start pipeline {} ({}): {e:#}", r.name, other.display());
                }
            }
        }

        self.account.remember(Recent { repo, data });
        self.emit(UiEvent::Workspace);
        Ok(())
    }

    /// Adds a pipeline stored in `data` to the current repository, records it
    /// in the pipelines file of every known pipeline, and switches to it.
    pub async fn add_pipeline(&self, name: &str, data: &Path) -> Result<()> {
        // The lock is released before `open`, which takes it itself.
        let (repo, data) = {
            let _guard = self.switching.lock().await;
            let app = self.app().ok_or_else(|| anyhow!("no repository is open"))?;
            let repo = app.layout.repo.clone();
            let mut list = pipelines(&repo, &app.layout.data).pipelines;
            let name = check_name(name, &list, None)?;
            if data.as_os_str().is_empty() {
                bail!("choose a folder for the new pipeline");
            }
            let data = if data.is_absolute() { data.to_path_buf() } else { repo.join(data) };
            std::fs::create_dir_all(&data).with_context(|| format!("creating {}", data.display()))?;
            let data = std::fs::canonicalize(&data)?;
            if data == repo {
                bail!("the repository itself can't be a pipeline folder; choose or create a subfolder");
            }
            if let Some(r) = list.iter().find(|r| r.data == data) {
                bail!("{} already holds the pipeline \"{}\"", data.display(), r.name);
            }
            list.push(PipelineRef { name: name.to_string(), data: data.clone() });
            sync_pipelines(&list)?;
            (repo, data)
        };
        self.open(&repo, Some(&data)).await
    }

    /// Renames the current pipeline (its folder stays where it is).
    pub async fn rename_pipeline(&self, name: &str) -> Result<()> {
        let _guard = self.switching.lock().await;
        let app = self.app().ok_or_else(|| anyhow!("no repository is open"))?;
        let data = &app.layout.data;
        let mut list = pipelines(&app.layout.repo, data).pipelines;
        let name = check_name(name, &list, Some(data.as_path()))?;
        for r in list.iter_mut().filter(|r| &r.data == data) {
            r.name = name.to_string();
        }
        sync_pipelines(&list)?;
        // Every pipeline's dropdown shows the new name.
        self.emit(UiEvent::Pipeline);
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

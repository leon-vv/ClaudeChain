//! Which repositories are open. The server can start without one and the
//! user opens them from the UI. Several can be open at once: each browser tab
//! looks at one of them (chosen by its id in the page URL), and every open
//! repository keeps scheduling agents whether or not a tab shows it. Recent
//! choices are remembered by `Account`.
//!
//! A repository can have several pipelines, each in its own data directory and
//! each an open app of its own (own id, own scheduler). A tab shows one of them
//! and the pipeline dropdown moves the tab to another; all of them keep running.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::account::{Account, Recent};
use crate::git;
use crate::orchestrator::{App, Envelope, UiEvent};
use crate::store::Layout;
use crate::sysmon::Monitor;

pub struct Workspace {
    claude: String,
    codex: String,
    pub events: broadcast::Sender<Envelope>,
    /// Open repositories by id.
    apps: RwLock<BTreeMap<String, Arc<App>>>,
    /// The most recently opened repository, for tabs that haven't picked one.
    default: RwLock<Option<String>>,
    /// Serializes open/switch so two requests can't race.
    switching: tokio::sync::Mutex<()>,
    pub account: Arc<Account>,
    pub monitor: Monitor,
    /// The Codex session log last read for plan usage, and its mtime.
    codex_log: Mutex<Option<(PathBuf, SystemTime)>>,
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
    pub fn new(claude: String, codex: String) -> Arc<Self> {
        let (events, _) = broadcast::channel(4096);
        Arc::new(Self {
            claude,
            codex,
            events,
            apps: RwLock::new(BTreeMap::new()),
            default: RwLock::new(None),
            switching: tokio::sync::Mutex::new(()),
            account: Account::load(),
            monitor: Monitor::new(),
            codex_log: Mutex::new(None),
        })
    }

    /// Reads Codex's plan usage from its newest session log if that changed
    /// since the last call, and tells the UI when it is newer.
    pub fn poll_codex_limits(&self) {
        let Some(newest) = crate::codex::newest_rollout() else { return };
        {
            let mut last = self.codex_log.lock().unwrap();
            if last.as_ref() == Some(&newest) {
                return;
            }
            *last = Some(newest.clone());
        }
        let Some((at, info)) = crate::codex::read_limits(&newest.0) else { return };
        if self.account.record_codex_limits(at, info) {
            self.emit_limits();
        }
    }

    /// Re-reads Codex's plan usage from scratch.
    pub fn refresh_codex_limits(&self) {
        self.account.forget_codex_limits();
        *self.codex_log.lock().unwrap() = None;
        self.poll_codex_limits();
    }

    /// Sends the account state (limits, pause) to every tab.
    pub fn emit_limits(&self) {
        let account = crate::orchestrator::account_json(&self.account);
        let _ = self.events.send(crate::orchestrator::Envelope::global(crate::orchestrator::UiEvent::Limits { account }));
    }

    /// Samples system resources forever (for the Resources page).
    pub async fn monitor_loop(self: Arc<Self>) {
        let mut last_disk: Option<std::time::Instant> = None;
        let mut tick: u64 = 0;
        loop {
            // Codex reports plan usage only in its session logs; check them
            // every few ticks (cheap when nothing changed).
            if tick.is_multiple_of(5) {
                let ws = self.clone();
                let _ = tokio::task::spawn_blocking(move || ws.poll_codex_limits()).await;
            }
            tick += 1;
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
        self.open(&r.repo, Some(&r.data), main, false).await?;
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
    ///
    /// With `init`, a folder that is not a repository gets `git init`, and a
    /// repository without commits gets an empty first commit. Without it those
    /// cases fail with a [`RepoSetup`] error so the UI can ask the user.
    ///
    /// The other pipelines listed in the data directory's `pipelines.json`
    /// are opened too (in the background), so their queued items progress.
    pub async fn open(&self, repo: &Path, data: Option<&Path>, main: Option<&str>, init: bool) -> Result<String> {
        let id = self.open_one(repo, data, main, false, init).await?;
        if let Some(app) = self.app(Some(&id)) {
            self.open_siblings(&app).await;
        }
        Ok(id)
    }

    async fn open_siblings(&self, app: &Arc<App>) {
        for r in pipelines(&app.layout.repo, &app.layout.data).pipelines {
            if r.data != app.layout.data
                && r.data.is_dir()
                && let Err(e) = self.open_one(&app.layout.repo, Some(&r.data), Some(&app.layout.main), true, false).await
            {
                tracing::warn!("could not start pipeline {} ({}): {e:#}", r.name, r.data.display());
            }
        }
    }

    /// Opens one repository + data directory. A `background` one (a sibling
    /// pipeline) isn't made the default or remembered as recent. See
    /// [`Workspace::open`] for `init`.
    async fn open_one(
        &self,
        repo: &Path,
        data: Option<&Path>,
        main: Option<&str>,
        background: bool,
        init: bool,
    ) -> Result<String> {
        let _guard = self.switching.lock().await;
        let repo = std::fs::canonicalize(repo).with_context(|| format!("{} does not exist", repo.display()))?;
        if git::check_repo(&repo).await.is_err() {
            if !init {
                return Err(RepoSetup::NotRepo(repo).into());
            }
            git::init(&repo).await.with_context(|| format!("git init in {}", repo.display()))?;
            tracing::info!("ran git init in {}", repo.display());
        }
        // Right after `git init` HEAD names a branch that doesn't exist yet,
        // so the main-branch check below would fail with a confusing error.
        if !git::has_commits(&repo).await? {
            if !init {
                return Err(RepoSetup::NoCommits(repo).into());
            }
            git::initial_commit(&repo)
                .await
                .with_context(|| format!("creating the first commit in {}", repo.display()))?;
            tracing::info!("created an empty first commit in {}", repo.display());
        }
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
                if !background {
                    *self.default.write().unwrap() = Some(id.clone());
                    self.account.remember(Recent { repo, data, main });
                    let _ = self.events.send(Envelope::global(UiEvent::Workspace));
                }
                return Ok(id);
            }
        if let Some(other) = self.apps().into_iter().find(|a| a.layout.data == data && a.id != id) {
            bail!("data directory {} is already used by {}", data.display(), other.layout.repo.display());
        }
        git::exclude_path(&repo, &data).await?;
        // Make sure the folder names itself and lists the pipelines it knows.
        let list = pipelines(&repo, &data);
        if read_pipelines(&data).is_none_or(|p| p.name != list.name || p.pipelines.len() != list.pipelines.len()) {
            write_pipelines(&data, &list)?;
        }
        let layout = Layout { repo: repo.clone(), data: data.clone(), main: main.clone() };
        // Another main branch for an open repository: replace its app.
        if let Some(old) = &existing {
            old.close()?;
        }
        let app = App::new(id.clone(), layout, self.claude.clone(), self.codex.clone(), self.events.clone(), self.account.clone())?;
        tokio::spawn(app.clone().scheduler());
        self.apps.write().unwrap().insert(id.clone(), app);
        tracing::info!("opened repo {} (data {}, main branch {main}, id {id})", repo.display(), data.display());
        if !background {
            *self.default.write().unwrap() = Some(id.clone());
            self.account.remember(Recent { repo, data, main });
            let _ = self.events.send(Envelope::global(UiEvent::Workspace));
        }
        Ok(id)
    }

    /// Adds a pipeline stored in `data` to the repository of `cur`, records it
    /// in the pipelines file of every known pipeline, opens it and returns its id.
    pub async fn add_pipeline(&self, cur: &Arc<App>, name: &str, data: &Path) -> Result<String> {
        // The lock is released before `open`, which takes it itself.
        let data = {
            let _guard = self.switching.lock().await;
            let repo = &cur.layout.repo;
            let mut list = pipelines(repo, &cur.layout.data).pipelines;
            let name = check_name(name, &list, None)?;
            if data.as_os_str().is_empty() {
                bail!("choose a folder for the new pipeline");
            }
            let data = if data.is_absolute() { data.to_path_buf() } else { repo.join(data) };
            std::fs::create_dir_all(&data).with_context(|| format!("creating {}", data.display()))?;
            let data = std::fs::canonicalize(&data)?;
            if data == *repo {
                bail!("the repository itself can't be a pipeline folder; choose or create a subfolder");
            }
            if let Some(r) = list.iter().find(|r| r.data == data) {
                bail!("{} already holds the pipeline \"{}\"", data.display(), r.name);
            }
            list.push(PipelineRef { name: name.to_string(), data: data.clone() });
            sync_pipelines(&list)?;
            data
        };
        self.open(&cur.layout.repo, Some(&data), Some(&cur.layout.main), false).await
    }

    /// Renames the pipeline of `cur` (its folder stays where it is).
    pub async fn rename_pipeline(&self, cur: &Arc<App>, name: &str) -> Result<()> {
        let _guard = self.switching.lock().await;
        let data = &cur.layout.data;
        let mut list = pipelines(&cur.layout.repo, data).pipelines;
        let name = check_name(name, &list, Some(data.as_path()))?;
        for r in list.iter_mut().filter(|r| &r.data == data) {
            r.name = name.to_string();
        }
        sync_pipelines(&list)?;
        // Every tab's dropdown shows the new name.
        let _ = self.events.send(Envelope::global(UiEvent::Pipeline));
        Ok(())
    }
}

/// A folder that needs setup before ClaudeChain can use it. The web API turns
/// this into an error `code` so the UI can offer to fix it.
#[derive(Debug)]
pub enum RepoSetup {
    NotRepo(PathBuf),
    NoCommits(PathBuf),
}

impl RepoSetup {
    pub fn code(&self) -> &'static str {
        match self {
            RepoSetup::NotRepo(_) => "not_repo",
            RepoSetup::NoCommits(_) => "no_commits",
        }
    }
}

impl std::fmt::Display for RepoSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoSetup::NotRepo(p) => write!(f, "{} is not a git repository", p.display()),
            RepoSetup::NoCommits(p) => write!(f, "{} has no commits yet", p.display()),
        }
    }
}

impl std::error::Error for RepoSetup {}

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

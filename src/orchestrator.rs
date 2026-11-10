//! Owns all state: the pipeline, the work items and the running agents.
//! A scheduler loop starts queued items whenever a step has a free worker.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast, mpsc, oneshot};

use crate::account::Account;
use crate::config::{self, Pipeline, Step};
use crate::git;
use crate::runner::{self, RunControl, RunSpec};
use crate::store::{self, Item, Layout, Note, PendingResume, Run, Status, TokenMeter, Transition};

/// Events pushed to the browser over SSE.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiEvent {
    Item { item: Box<Item> },
    ItemRemoved { id: String },
    Pipeline,
    /// Plan usage, the pause threshold, or the paused state changed.
    Limits { account: Value },
    /// A different repository was opened (or none is open).
    Workspace,
    RunEvent { item: String, run: u32, event: Value },
}

pub struct App {
    pub layout: Layout,
    pub claude: String,
    pub pipeline: RwLock<Pipeline>,
    pub items: Mutex<BTreeMap<String, Item>>,
    running: Mutex<HashMap<String, RunControl>>,
    pub events: broadcast::Sender<UiEvent>,
    wake: Notify,
    /// Set when another repository is opened; the scheduler stops.
    closed: AtomicBool,
    pub account: Arc<Account>,
    /// Last reason the scheduler held runs back, to notice changes.
    paused: Mutex<Option<String>>,
}

/// A running agent, for the Resources page.
#[derive(Debug, Clone, Serialize)]
pub struct Agent {
    pub item: String,
    pub title: String,
    pub step: String,
    pub run: u32,
    pub pid: u32,
}

/// Account-level usage info as sent to the UI.
pub fn account_json(account: &Account) -> Value {
    json!({
        "limits": account.limits(),
        "pause_above": account.pause_above(),
        "paused": account.paused_reason(),
    })
}

/// What the human wants to do with a message typed in the run inspector.
#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageMode {
    /// Deliver into the running agent's session.
    Live,
    /// Resume a finished run's session with this message.
    Continue,
    /// Keep for the next run of this item.
    Note,
}

impl App {
    pub fn new(
        layout: Layout,
        claude: String,
        events: broadcast::Sender<UiEvent>,
        account: Arc<Account>,
    ) -> Result<Arc<Self>> {
        let pipeline = config::load_or_init(&layout.data)?;
        let mut items = store::load_items(&layout)?;
        // Runs from before token tracking: count their tokens from the transcript.
        for item in items.values_mut() {
            let mut changed = false;
            for run in item.runs.iter_mut().filter(|r| r.tokens.is_none() && r.ended.is_some()) {
                let events = layout.run_dir(&item.id, run.n, &run.step).join("events.jsonl");
                let Ok(text) = std::fs::read_to_string(events) else { continue };
                let mut meter = TokenMeter::default();
                for line in text.lines() {
                    if let Ok(ev) = serde_json::from_str::<Value>(line) {
                        meter.observe(&ev);
                    }
                }
                run.tokens = Some(meter.total());
                changed = true;
            }
            if changed {
                store::save_item(&layout, item)?;
            }
        }
        // Agents don't survive a restart: requeue whatever was running.
        for item in items.values_mut() {
            if item.status == Status::Running {
                if let Some(run) = item.runs.last_mut().filter(|r| r.ended.is_none()) {
                    run.ended = Some(Utc::now());
                    run.failed = true;
                    run.outcome = Some("Interrupted (ClaudeChain restarted)".into());
                }
                item.status = Status::Queued;
                item.approved = true;
                store::save_item(&layout, item)?;
            }
        }
        Ok(Arc::new(Self {
            layout,
            claude,
            pipeline: RwLock::new(pipeline),
            items: Mutex::new(items),
            running: Mutex::new(HashMap::new()),
            events,
            wake: Notify::new(),
            closed: AtomicBool::new(false),
            account,
            paused: Mutex::new(None),
        }))
    }

    pub fn pipeline(&self) -> Pipeline {
        self.pipeline.read().unwrap().clone()
    }

    fn emit(&self, ev: UiEvent) {
        let _ = self.events.send(ev);
    }

    /// Applies `f` to an item, saves it and notifies the UI and scheduler.
    pub fn update_item<T>(&self, id: &str, f: impl FnOnce(&mut Item) -> Result<T>) -> Result<T> {
        let mut items = self.items.lock().unwrap();
        let item = items.get_mut(id).ok_or_else(|| anyhow!("no item '{id}'"))?;
        let out = f(item)?;
        store::save_item(&self.layout, item)?;
        self.emit(UiEvent::Item { item: Box::new(item.clone()) });
        drop(items);
        self.wake.notify_one();
        Ok(out)
    }

    /// Like `update_item`, but only in memory (for frequent live updates);
    /// the item is saved when its run finishes.
    fn update_item_live(&self, id: &str, f: impl FnOnce(&mut Item)) {
        let mut items = self.items.lock().unwrap();
        if let Some(item) = items.get_mut(id) {
            f(item);
            self.emit(UiEvent::Item { item: Box::new(item.clone()) });
        }
    }

    pub fn agents(&self) -> Vec<Agent> {
        let running = self.running.lock().unwrap();
        let items = self.items.lock().unwrap();
        running
            .iter()
            .filter_map(|(id, control)| {
                let item = items.get(id)?;
                let run = item.runs.last()?;
                Some(Agent {
                    item: id.clone(),
                    title: item.title.clone(),
                    step: run.step.clone(),
                    run: run.n,
                    pid: control.pid.load(std::sync::atomic::Ordering::Relaxed),
                })
            })
            .filter(|a| a.pid != 0)
            .collect()
    }

    pub fn emit_account(&self) {
        self.emit(UiEvent::Limits { account: account_json(&self.account) });
    }

    pub fn update_pipeline(&self, f: impl FnOnce(&mut Pipeline) -> Result<()>) -> Result<()> {
        let mut p = self.pipeline.write().unwrap();
        let mut next = p.clone();
        f(&mut next)?;
        config::save(&self.layout.data, &next)?;
        *p = next;
        drop(p);
        self.emit(UiEvent::Pipeline);
        self.wake.notify_one();
        Ok(())
    }

    // ---- Adding and removing items -------------------------------------

    /// Adds an item from a GitHub issue reference or from pasted issue text.
    pub async fn add_item(&self, input: &str) -> Result<Item> {
        let input = input.trim();
        if input.is_empty() {
            bail!("empty issue");
        }
        let (id, number, title, url, body) = if let Some(r) = git::issue_ref(input) {
            let issue = git::fetch_issue(&self.layout.repo, &r)
                .await
                .with_context(|| format!("gh issue view {r}"))?;
            let labels: Vec<_> = issue.labels.iter().map(|l| l.name.as_str()).collect();
            let mut body = format!("# #{} {}\n\n{}\n", issue.number, issue.title, issue.url);
            if !labels.is_empty() {
                body.push_str(&format!("\nLabels: {}\n", labels.join(", ")));
            }
            body.push_str(&format!("\n{}\n", issue.body));
            (format!("issue-{}", issue.number), Some(issue.number), issue.title, Some(issue.url), body)
        } else {
            let title = input.lines().next().unwrap_or("").trim_start_matches('#').trim();
            let title: String = title.chars().take(120).collect();
            let items = self.items.lock().unwrap();
            let n = (1..).find(|n| !items.contains_key(&format!("item-{n}"))).unwrap();
            (format!("item-{n}"), None, title, None, format!("{input}\n"))
        };

        let first = self.pipeline().steps[0].id.clone();
        let now = Utc::now();
        let item = Item {
            branch: format!("claudechain/{id}"),
            id: id.clone(),
            number,
            title,
            url,
            created: now,
            step: first,
            status: Status::Queued,
            status_detail: None,
            since: now,
            iteration: 0,
            approved: false,
            runs: vec![],
            pending_notes: vec![],
            pending_resume: None,
            base_commit: None,
            pr_url: None,
        };
        {
            let mut items = self.items.lock().unwrap();
            if items.contains_key(&id) {
                bail!("{id} is already in the pipeline");
            }
            store::write_atomic(&self.layout.issue_md(&id), body.as_bytes())?;
            std::fs::create_dir_all(self.layout.item_dir(&id).join("messages"))?;
            store::save_item(&self.layout, &item)?;
            items.insert(id.clone(), item.clone());
        }
        self.emit(UiEvent::Item { item: Box::new(item.clone()) });
        self.wake.notify_one();
        Ok(item)
    }

    /// Removes an item from the board. Its folder is moved to `archive/`;
    /// the worktree is removed but the branch is kept.
    pub async fn remove_item(&self, id: &str) -> Result<()> {
        self.stop(id).ok();
        let item = self.items.lock().unwrap().remove(id).ok_or_else(|| anyhow!("no item '{id}'"))?;
        if let Some(c) = self.running.lock().unwrap().remove(id) {
            drop(c);
        }
        if let Err(e) = git::remove_worktree(&self.layout.repo, &self.layout.worktree(id)).await {
            tracing::warn!("removing worktree for {id}: {e}");
        }
        let archive = self.layout.data.join("archive");
        std::fs::create_dir_all(&archive)?;
        let target = archive.join(format!("{}-{}", item.id, Utc::now().format("%Y%m%d%H%M%S")));
        std::fs::rename(self.layout.item_dir(id), target)?;
        self.emit(UiEvent::ItemRemoved { id: id.to_string() });
        Ok(())
    }

    // ---- Human controls ----------------------------------------------

    pub fn approve(&self, id: &str) -> Result<()> {
        self.update_item(id, |item| {
            if item.status != Status::AwaitingApproval {
                bail!("{id} is not waiting for approval");
            }
            item.approved = true;
            item.set_status(Status::Queued, None);
            Ok(())
        })
    }

    /// Manually moves an item to a step (or "done"), with an optional note.
    pub fn move_item(&self, id: &str, step: &str, note: Option<String>) -> Result<()> {
        let pipeline = self.pipeline();
        if step != "done" && pipeline.step(step).is_none() {
            bail!("unknown step '{step}'");
        }
        self.update_item(id, |item| {
            if item.status == Status::Running {
                bail!("{id} is running; stop it first");
            }
            item.pending_resume = None;
            if let Some(text) = note.filter(|t| !t.trim().is_empty()) {
                item.pending_notes.push(Note { at: Utc::now(), text });
            }
            if step == "done" {
                item.set_status(Status::Done, Some("Marked done by the human".into()));
            } else {
                item.enter_step(step);
                // A human decided this move, so skip the approval gate.
                item.approved = true;
            }
            Ok(())
        })
    }

    pub fn retry(&self, id: &str) -> Result<()> {
        self.update_item(id, |item| {
            if item.status == Status::Running {
                bail!("{id} is already running");
            }
            let step = item.step.clone();
            item.pending_resume = None;
            item.enter_step(&step);
            item.approved = true;
            Ok(())
        })
    }

    pub fn stop(&self, id: &str) -> Result<()> {
        let mut running = self.running.lock().unwrap();
        let control = running.get_mut(id).ok_or_else(|| anyhow!("{id} is not running"))?;
        if let Some(stop) = control.stop.take() {
            let _ = stop.send(());
        }
        Ok(())
    }

    pub fn send_message(&self, id: &str, run: Option<u32>, mode: MessageMode, text: String) -> Result<()> {
        if text.trim().is_empty() {
            bail!("empty message");
        }
        match mode {
            MessageMode::Live => {
                let running = self.running.lock().unwrap();
                let control = running.get(id).ok_or_else(|| anyhow!("{id} is not running"))?;
                control.messages.send(text).map_err(|_| anyhow!("the agent has finished"))?;
                Ok(())
            }
            MessageMode::Note => self.update_item(id, |item| {
                item.pending_notes.push(Note { at: Utc::now(), text });
                Ok(())
            }),
            MessageMode::Continue => self.update_item(id, |item| {
                if item.status == Status::Running {
                    bail!("{id} is running; send the message live instead");
                }
                let n = run.or(item.runs.last().map(|r| r.n)).ok_or_else(|| anyhow!("no run to continue"))?;
                let r = item.run(n).ok_or_else(|| anyhow!("no run {n}"))?;
                if r.session_id.is_none() {
                    bail!("run {n} has no session to resume");
                }
                let step = r.step.clone();
                item.enter_step(&step);
                item.approved = true;
                item.pending_resume = Some(PendingResume { from_run: n, message: text });
                Ok(())
            }),
        }
    }

    // ---- Scheduling ------------------------------------------------------

    /// Stops scheduling so another repository can be opened. Fails while
    /// agents are running.
    pub fn close(&self) -> Result<()> {
        let items = self.items.lock().unwrap();
        let running: Vec<_> = items.values().filter(|i| i.status == Status::Running).map(|i| i.id.as_str()).collect();
        if !running.is_empty() {
            bail!("agents are still running ({}); stop them first", running.join(", "));
        }
        self.closed.store(true, Ordering::SeqCst);
        drop(items);
        self.wake.notify_one();
        Ok(())
    }

    /// Replaces the whole pipeline (steps, transitions, settings) from the UI.
    pub fn replace_pipeline(&self, next: Pipeline) -> Result<()> {
        next.validate()?;
        {
            // Every unfinished item must still have a step to be in.
            let items = self.items.lock().unwrap();
            let orphans: Vec<_> = items
                .values()
                .filter(|i| i.status != Status::Done && next.step(&i.step).is_none())
                .map(|i| format!("{} ({})", i.id, i.step))
                .collect();
            if !orphans.is_empty() {
                bail!("these items are in a step you removed; move them first: {}", orphans.join(", "));
            }
        }
        for step in &next.steps {
            if !config::prompt_path(&self.layout.data, &step.id).exists() {
                config::write_prompt(&self.layout.data, &step.id, &config::stub_prompt(&step.name))?;
            }
        }
        self.update_pipeline(|p| {
            *p = next;
            Ok(())
        })
    }

    pub async fn scheduler(self: Arc<Self>) {
        while !self.closed.load(Ordering::SeqCst) {
            self.schedule();
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
    }

    fn schedule(self: &Arc<Self>) {
        let pipeline = self.pipeline();
        // Hold back new runs while the plan usage is above the threshold.
        let paused = self.account.paused_reason();
        if *self.paused.lock().unwrap() != paused {
            if let Some(why) = &paused {
                tracing::info!("holding new runs: {why}");
            }
            *self.paused.lock().unwrap() = paused.clone();
            self.emit_account();
        }
        let mut to_start = vec![];
        {
            let mut items = self.items.lock().unwrap();
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            for step in &pipeline.steps {
                let mut busy = items.values().filter(|i| i.status == Status::Running && i.step == step.id).count();
                let mut queue: Vec<&mut Item> = items
                    .values_mut()
                    .filter(|i| i.step == step.id && matches!(i.status, Status::Queued | Status::AwaitingApproval))
                    .collect();
                queue.sort_by_key(|i| i.since);
                for item in queue {
                    let gated = step.require_approval && !item.approved && item.pending_resume.is_none();
                    let changed = match (item.status, gated) {
                        (Status::Queued, true) => {
                            item.set_status(Status::AwaitingApproval, None);
                            true
                        }
                        (Status::AwaitingApproval, false) => {
                            item.set_status(Status::Queued, None);
                            true
                        }
                        _ => false,
                    };
                    if item.status == Status::Queued && busy < step.max_workers && paused.is_none() {
                        busy += 1;
                        let n = item.next_run_number();
                        item.runs.push(Run {
                            n,
                            step: step.id.clone(),
                            model: step.model.clone(),
                            started: Utc::now(),
                            ended: None,
                            resumed_from: item.pending_resume.as_ref().map(|r| r.from_run),
                            session_id: None,
                            cost_usd: None,
                            tokens: None,
                            transition: None,
                            outcome: None,
                            failed: false,
                        });
                        item.set_status(Status::Running, None);
                        to_start.push((item.id.clone(), n));
                    } else if !changed {
                        continue;
                    }
                    if let Err(e) = store::save_item(&self.layout, item) {
                        tracing::error!("saving {}: {e:#}", item.id);
                    }
                    self.emit(UiEvent::Item { item: Box::new(item.clone()) });
                }
            }
        }
        for (id, n) in to_start {
            let app = self.clone();
            tokio::spawn(async move { app.execute(id, n).await });
        }
    }

    // ---- Running one agent ---------------------------------------------

    async fn execute(self: Arc<Self>, id: String, n: u32) {
        let result = self.clone().execute_inner(&id, n).await;
        let pipeline = self.pipeline();
        self.running.lock().unwrap().remove(&id);
        let finish = self.update_item(&id, |item| {
            finish_run(&self.layout, &pipeline, item, n, result);
            Ok(())
        });
        if let Err(e) = finish {
            tracing::error!("finishing {id} run {n}: {e:#}");
        }
    }

    async fn execute_inner(self: Arc<Self>, id: &str, n: u32) -> Result<runner::RunResult> {
        let pipeline = self.pipeline();
        let item = self.items.lock().unwrap().get(id).cloned().ok_or_else(|| anyhow!("item vanished"))?;
        let run = item.run(n).cloned().ok_or_else(|| anyhow!("run vanished"))?;
        let step = pipeline.step(&run.step).cloned().ok_or_else(|| anyhow!("unknown step {}", run.step))?;

        let worktree = self.layout.worktree(id);
        let head = git::ensure_worktree(&self.layout.repo, &worktree, &item.branch)
            .await
            .context("creating git worktree")?;
        if item.base_commit.is_none() {
            self.update_item(id, |i| {
                i.base_commit = Some(head);
                Ok(())
            })?;
        }

        let run_dir = self.layout.run_dir(id, n, &step.id);
        std::fs::create_dir_all(&run_dir)?;
        std::fs::create_dir_all(self.layout.item_dir(id).join("messages"))?;

        let resume = item.pending_resume.clone().filter(|r| r.from_run == run.resumed_from.unwrap_or(0));
        let resume_session = resume
            .as_ref()
            .and_then(|r| item.run(r.from_run))
            .and_then(|r| r.session_id.clone());
        let notes = item.pending_notes.clone();

        let system_prompt = format!(
            "{}\n\n# Step: {}\n\n{}",
            config::read_prompt(&self.layout.data, "general").trim(),
            step.name,
            config::read_prompt(&self.layout.data, &step.id).trim(),
        );
        let prompt = match &resume {
            Some(r) => resume_prompt(&self.layout, &pipeline, &item, &step, n, r, &notes),
            None => initial_prompt(&self.layout, &pipeline, &item, &step, n, &notes),
        };

        let tools = step.tools.join(",");
        let mut args: Vec<String> = [
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            // Only the built-in tools: no MCP servers from the user's own config.
            "--strict-mcp-config",
            "--model",
            &step.model,
            "--permission-mode",
            &step.permission_mode,
            "--tools",
            &tools,
            "--allowedTools",
            &tools,
            "--append-system-prompt",
            &system_prompt,
            "--add-dir",
        ]
        .map(String::from)
        .to_vec();
        args.push(self.layout.item_dir(id).display().to_string());
        if let Some(sid) = &resume_session {
            args.push("--resume".into());
            args.push(sid.clone());
        }

        let input = json!({
            "step": step.id,
            "model": step.model,
            "tools": step.tools,
            "permission_mode": step.permission_mode,
            "cwd": worktree,
            "resume_session": resume_session,
            "notes": notes,
            "system_prompt": system_prompt,
            "prompt": prompt,
            "command": std::iter::once(self.claude.clone()).chain(args.iter().cloned()).collect::<Vec<_>>(),
        });
        store::write_atomic(&run_dir.join("input.json"), serde_json::to_string_pretty(&input)?.as_bytes())?;

        // The notes and resume request are now consumed by this run.
        self.update_item(id, |i| {
            i.pending_notes.clear();
            i.pending_resume = None;
            Ok(())
        })?;

        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = oneshot::channel();
        let pid = Arc::new(std::sync::atomic::AtomicU32::new(0));
        self.running
            .lock()
            .unwrap()
            .insert(id.to_string(), RunControl { messages: msg_tx, stop: Some(stop_tx), pid: pid.clone() });

        let spec = RunSpec {
            program: self.claude.clone(),
            args,
            cwd: worktree,
            initial_prompt: prompt,
            events_path: run_dir.join("events.jsonl"),
            stderr_path: run_dir.join("stderr.log"),
            pid,
        };
        let item_id = id.to_string();
        let app = self.clone();
        let mut session_recorded = false;
        let mut meter = TokenMeter::default();
        runner::run(spec, msg_rx, stop_rx, move |event| {
            if meter.observe(event) {
                let tokens = meter.total();
                app.update_item_live(&item_id, |i| {
                    if let Some(r) = i.run_mut(n) {
                        r.tokens = Some(tokens);
                    }
                });
            }
            if event["type"] == "rate_limit_event"
                && let Some(info) = event.get("rate_limit_info")
                && app.account.record_limits(info)
            {
                app.emit_account();
                app.wake.notify_one();
            }
            if !session_recorded
                && let Some(sid) = event.get("session_id").and_then(Value::as_str) {
                    session_recorded = true;
                    let sid = sid.to_string();
                    let _ = app.update_item(&item_id, |i| {
                        if let Some(r) = i.run_mut(n) {
                            r.session_id = Some(sid);
                        }
                        Ok(())
                    });
                }
            app.emit(UiEvent::RunEvent { item: item_id.clone(), run: n, event: event.clone() });
        })
        .await
    }
}

/// Records the run's outcome and moves the item according to its transition.
fn finish_run(layout: &Layout, pipeline: &Pipeline, item: &mut Item, n: u32, result: Result<runner::RunResult>) {
    let step_id = item.run(n).map(|r| r.step.clone()).unwrap_or_else(|| item.step.clone());
    let transition_path = layout.transition_file(&item.id, n, &step_id);
    let parsed = std::fs::read_to_string(&transition_path)
        .ok()
        .map(|t| serde_json::from_str::<Transition>(&t).map_err(|e| format!("invalid transition.json: {e}")));
    let transition = parsed.as_ref().and_then(|t| t.as_ref().ok()).cloned();

    let mut cost = None;
    let mut session = None;
    // Decide where the item goes: Ok(Some(step)), Ok(None) = done, Err(reason) = human.
    let decision: Result<Option<String>, String> = match result {
        Err(e) => Err(format!("Agent could not run: {e:#}")),
        Ok(res) => {
            cost = res.cost_usd;
            session = res.session_id.clone();
            if res.stopped {
                Err("Stopped by the human".into())
            } else {
                match parsed {
                    Some(Ok(t)) => resolve_target(pipeline, &step_id, &t),
                    Some(Err(e)) => Err(e),
                    None => {
                        let mut why = "Agent finished without writing transition.json".to_string();
                        if res.is_error || res.exit.is_some() {
                            why = format!("Agent failed ({})", res.exit.as_deref().unwrap_or("error result"));
                        }
                        if let Some(text) = res.result_text.filter(|t| !t.is_empty()) {
                            let short: String = text.chars().take(300).collect();
                            why.push_str(&format!(": {short}"));
                        }
                        Err(why)
                    }
                }
            }
        }
    };

    if let Some(pr) = transition.as_ref().and_then(|t| t.pr_url.clone()) {
        item.pr_url = Some(pr);
    }
    let from_idx = pipeline.step_index(&step_id).unwrap_or(0);
    let outcome = match &decision {
        Ok(None) => "→ done".to_string(),
        Ok(Some(to)) => format!("→ {}", pipeline.step(to).map_or(to.as_str(), |s| &s.name)),
        Err(why) => format!("→ human: {why}"),
    };
    if let Some(run) = item.run_mut(n) {
        run.ended = Some(Utc::now());
        run.cost_usd = cost.or(run.cost_usd);
        if session.is_some() {
            run.session_id = session;
        }
        run.failed = decision.is_err();
        run.outcome = Some(outcome);
        run.transition = transition.clone();
    }

    let summary = transition.map(|t| t.summary).filter(|s| !s.is_empty());
    match decision {
        Ok(None) => item.set_status(Status::Done, summary),
        Ok(Some(to)) => {
            let backward = pipeline.step_index(&to).is_some_and(|i| i <= from_idx);
            if backward {
                item.iteration += 1;
            }
            item.enter_step(&to);
            if backward && item.iteration > pipeline.max_iterations {
                item.set_status(
                    Status::NeedsHuman,
                    Some(format!("Iteration limit ({}) reached; last: {}", pipeline.max_iterations, summary.unwrap_or_default())),
                );
            } else {
                item.status_detail = summary;
            }
        }
        Err(why) => {
            let detail = match summary {
                Some(s) if why.starts_with("Agent asked") => format!("{why}: {s}"),
                _ => why,
            };
            item.set_status(Status::NeedsHuman, Some(detail));
        }
    }
}

fn resolve_target(pipeline: &Pipeline, from: &str, t: &Transition) -> Result<Option<String>, String> {
    let step = pipeline.step(from).ok_or("step no longer exists")?;
    if !step.allowed_targets.iter().any(|a| a == &t.to) {
        return Err(format!(
            "Agent chose transition '{}', which is not allowed from {} (allowed: {})",
            t.to,
            step.name,
            step.allowed_targets.join(", ")
        ));
    }
    match t.to.as_str() {
        "human" => Err("Agent asked for a human".into()),
        "next" => Ok(pipeline.next_step(from).map(|s| s.id.clone())),
        other => Ok(Some(other.to_string())),
    }
}

fn describe_targets(pipeline: &Pipeline, step: &Step) -> String {
    step.allowed_targets
        .iter()
        .map(|t| match t.as_str() {
            "next" => match pipeline.next_step(&step.id) {
                Some(s) => format!("- `\"next\"`: on to {}", s.name),
                None => "- `\"next\"`: the item is done".into(),
            },
            "human" => "- `\"human\"`: stop and ask the human".into(),
            id => format!("- `\"{id}\"`: send to {}", pipeline.step(id).map_or(id, |s| &s.name)),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn output_section(layout: &Layout, pipeline: &Pipeline, item: &Item, step: &Step, n: u32) -> String {
    format!(
        "## Your output\n\n- Message file: `{}`\n- Transition file: `{}`\n\nAllowed transitions:\n{}\n",
        layout.message_file(&item.id, n, &step.id).display(),
        layout.transition_file(&item.id, n, &step.id).display(),
        describe_targets(pipeline, step),
    )
}

fn notes_section(notes: &[Note]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut s = "## Notes from the human for this run\n\n".to_string();
    for note in notes {
        s.push_str(&format!("- ({}) {}\n", note.at.format("%Y-%m-%d %H:%M"), note.text.trim()));
    }
    s.push('\n');
    s
}

fn initial_prompt(layout: &Layout, pipeline: &Pipeline, item: &Item, step: &Step, n: u32, notes: &[Note]) -> String {
    let dir = layout.item_dir(&item.id);
    let mut messages: Vec<String> = std::fs::read_dir(dir.join("messages"))
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default();
    messages.sort();
    let messages = if messages.is_empty() {
        "(none yet: you are the first agent on this item)".to_string()
    } else {
        messages.iter().map(|m| format!("- `messages/{m}`")).collect::<Vec<_>>().join("\n")
    };
    let history = item
        .runs
        .iter()
        .filter(|r| r.n < n)
        .map(|r| {
            let name = pipeline.step(&r.step).map_or(r.step.as_str(), |s| &s.name);
            format!("- run {} {}: {}", r.n, name, r.outcome.as_deref().unwrap_or("(unfinished)"))
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "# Work item {id}: {title}\n\n\
         - Issue: {issue}\n\
         - Step: {step_name} (`{step_id}`), run {n}, iteration {iter} of max {max}\n\
         - Working directory: `{wt}` (git worktree, branch `{branch}`)\n\
         - Work item folder: `{dir}`\n\
         - Issue text: `{dir}/issue.md`\n\n\
         ## Earlier messages (oldest first, in the work item folder)\n\n{messages}\n\n\
         ## Pipeline history\n\n{history}\n\n\
         {notes}{output}\n\
         Start by reading the issue and the relevant earlier messages.",
        id = item.id,
        title = item.title,
        issue = item.url.clone().unwrap_or_else(|| "(pasted text, no GitHub issue)".into()),
        step_name = step.name,
        step_id = step.id,
        iter = item.iteration,
        max = pipeline.max_iterations,
        wt = layout.worktree(&item.id).display(),
        branch = item.branch,
        dir = dir.display(),
        history = if history.is_empty() { "(this is the first run)".into() } else { history },
        notes = notes_section(notes),
        output = output_section(layout, pipeline, item, step, n),
    )
}

fn resume_prompt(
    layout: &Layout,
    pipeline: &Pipeline,
    item: &Item,
    step: &Step,
    n: u32,
    resume: &PendingResume,
    notes: &[Note],
) -> String {
    format!(
        "Message from the human supervising this pipeline:\n\n{}\n\n---\n\n\
         This continues your earlier run as run {n}. Act on the message above, then write a \
         new message file and transition file at these new paths (the old ones are kept as history).\n\n{}{}",
        resume.message.trim(),
        notes_section(notes),
        output_section(layout, pipeline, item, step, n),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(name: &str) -> (Layout, Pipeline, Item) {
        let root = std::env::temp_dir().join(format!("claudechain-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let layout = Layout { repo: root.clone(), data: root.join("data") };
        let pipeline = config::load_or_init(&layout.data).unwrap();
        let now = Utc::now();
        let item = Item {
            id: "item-1".into(),
            number: None,
            title: "t".into(),
            url: None,
            branch: "claudechain/item-1".into(),
            created: now,
            step: "build_test".into(),
            status: Status::Running,
            status_detail: None,
            since: now,
            iteration: 0,
            approved: false,
            runs: vec![],
            pending_notes: vec![],
            pending_resume: None,
            base_commit: None,
            pr_url: None,
        };
        (layout, pipeline, item)
    }

    fn start_run(layout: &Layout, item: &mut Item, step: &str, transition: Option<&str>) -> u32 {
        let n = item.next_run_number();
        item.step = step.into();
        item.status = Status::Running;
        item.runs.push(Run {
            n,
            step: step.into(),
            model: "m".into(),
            started: Utc::now(),
            ended: None,
            resumed_from: None,
            session_id: None,
            cost_usd: None,
            tokens: None,
            transition: None,
            outcome: None,
            failed: false,
        });
        if let Some(t) = transition {
            store::write_atomic(&layout.transition_file(&item.id, n, step), t.as_bytes()).unwrap();
        }
        n
    }

    fn ok() -> Result<runner::RunResult> {
        Ok(runner::RunResult::default())
    }

    #[test]
    fn next_moves_forward() {
        let (layout, pipeline, mut item) = setup("forward");
        let n = start_run(&layout, &mut item, "build_test", Some(r#"{"to":"next","summary":"green"}"#));
        finish_run(&layout, &pipeline, &mut item, n, ok());
        assert_eq!((item.step.as_str(), item.status), ("review", Status::Queued));
        assert_eq!(item.iteration, 0);
        assert_eq!(item.status_detail.as_deref(), Some("green"));
        assert!(!item.run(n).unwrap().failed);
    }

    #[test]
    fn backward_move_counts_iterations_up_to_the_limit() {
        let (layout, mut pipeline, mut item) = setup("backward");
        pipeline.max_iterations = 1;
        let t = r#"{"to":"implementation","summary":"tests fail"}"#;
        let n = start_run(&layout, &mut item, "build_test", Some(t));
        finish_run(&layout, &pipeline, &mut item, n, ok());
        assert_eq!((item.step.as_str(), item.status, item.iteration), ("implementation", Status::Queued, 1));

        let n = start_run(&layout, &mut item, "build_test", Some(t));
        finish_run(&layout, &pipeline, &mut item, n, ok());
        assert_eq!((item.status, item.iteration), (Status::NeedsHuman, 2));
        assert!(item.status_detail.as_deref().unwrap().contains("Iteration limit"));
    }

    #[test]
    fn disallowed_or_missing_transition_needs_human() {
        let (layout, pipeline, mut item) = setup("invalid");
        let n = start_run(&layout, &mut item, "build_test", Some(r#"{"to":"triage"}"#));
        finish_run(&layout, &pipeline, &mut item, n, ok());
        assert_eq!(item.status, Status::NeedsHuman);
        assert!(item.run(n).unwrap().failed);

        let n = start_run(&layout, &mut item, "build_test", None);
        finish_run(&layout, &pipeline, &mut item, n, ok());
        assert_eq!(item.status, Status::NeedsHuman);
        assert!(item.status_detail.as_deref().unwrap().contains("without writing transition.json"));

        let n = start_run(&layout, &mut item, "build_test", Some("not json"));
        finish_run(&layout, &pipeline, &mut item, n, ok());
        assert!(item.status_detail.as_deref().unwrap().contains("invalid transition.json"));
    }

    #[test]
    fn human_transition_and_stop() {
        let (layout, pipeline, mut item) = setup("human");
        let n = start_run(&layout, &mut item, "triage", Some(r#"{"to":"human","summary":"unclear"}"#));
        finish_run(&layout, &pipeline, &mut item, n, ok());
        assert_eq!(item.status, Status::NeedsHuman);
        assert_eq!(item.status_detail.as_deref(), Some("Agent asked for a human: unclear"));

        let n = start_run(&layout, &mut item, "triage", Some(r#"{"to":"next"}"#));
        let stopped = runner::RunResult { stopped: true, ..Default::default() };
        finish_run(&layout, &pipeline, &mut item, n, Ok(stopped));
        assert_eq!(item.status, Status::NeedsHuman);
        assert_eq!(item.step, "triage");
    }

    #[test]
    fn next_from_last_step_is_done_and_records_pr() {
        let (layout, pipeline, mut item) = setup("done");
        let t = r#"{"to":"next","summary":"opened","pr_url":"https://github.com/o/r/pull/1"}"#;
        let n = start_run(&layout, &mut item, "pull_request", Some(t));
        finish_run(&layout, &pipeline, &mut item, n, ok());
        assert_eq!(item.status, Status::Done);
        assert_eq!(item.pr_url.as_deref(), Some("https://github.com/o/r/pull/1"));
    }

    #[test]
    fn issue_refs() {
        assert_eq!(git::issue_ref("#12").as_deref(), Some("12"));
        assert_eq!(git::issue_ref(" 7 ").as_deref(), Some("7"));
        assert!(git::issue_ref("https://github.com/o/r/issues/3").is_some());
        assert!(git::issue_ref("Fix the thing\n\nbody").is_none());
        assert!(git::issue_ref("#").is_none());
    }
}

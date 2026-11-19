//! A basic MCP (Model Context Protocol) server, so an agent (e.g. Claude Code
//! on another machine) can look at the board and drive work items.
//!
//! Streamable HTTP transport in its simplest form: the client POSTs one
//! JSON-RPC message to `/mcp` and gets a JSON response back (no SSE, no
//! sessions). Notifications get `202 Accepted` with an empty body.

use std::fmt::Write as _;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use crate::config::Pipeline;
use crate::orchestrator::{App, MessageMode};
use crate::store::{Item, Status};
use crate::workspace::Workspace;

const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub async fn handle(State(ws): State<Arc<Workspace>>, body: Bytes) -> Response {
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return Json(error_response(Value::Null, -32700, &format!("parse error: {e}"))).into_response(),
    };
    match dispatch(&ws, msg).await {
        Some(resp) => Json(resp).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Handles one JSON-RPC message. Returns `None` for notifications and
/// responses (messages without a method or without an id).
async fn dispatch(ws: &Workspace, msg: Value) -> Option<Value> {
    let Some(obj) = msg.as_object() else {
        return Some(error_response(Value::Null, -32600, "invalid request: expected a JSON-RPC object"));
    };
    let id = obj.get("id").cloned();
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        // A response from the client (we never send requests) or garbage.
        return id.map(|id| error_response(id, -32600, "invalid request: missing method"));
    };
    let id = id?; // Notification, e.g. notifications/initialized: nothing to answer.
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => Ok(call_tool(ws, &params).await),
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => error_response(id, code, &message),
    })
}

fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
    let version = SUPPORTED_VERSIONS.iter().find(|v| **v == requested).unwrap_or(&SUPPORTED_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "claudechain", "version": env!("CARGO_PKG_VERSION")},
        "instructions": "ClaudeChain runs work items (GitHub issues) through a pipeline of Claude agents. \
            Use `board` for an overview, `get_item` for details, and the other tools to approve, move, retry, \
            stop or message items. Several repositories can be open: `list_repos` lists them, and every tool \
            takes an optional `repo` argument.",
    })
}

/// Every tool takes an optional `repo`, since several repositories can be open.
fn schema(mut props: Value, required: &[&str]) -> Value {
    props["repo"] = json!({
        "type": "string",
        "description": "Repository: its id (see list_repos), path or directory name. Optional when it is \
            unambiguous; board and list_items cover all open repositories without it.",
    });
    json!({"type": "object", "properties": props, "required": required})
}

fn tools() -> Vec<Value> {
    let id = json!({"type": "string", "description": "Work item id, e.g. item-7"});
    vec![
        json!({
            "name": "list_repos",
            "description": "The repositories open in ClaudeChain (id, path, main branch, item count).",
            "inputSchema": {"type": "object", "properties": {}, "required": []},
        }),
        json!({
            "name": "board",
            "description": "Text overview of the board: every work item grouped by pipeline step, with status, \
                iteration, last run and PR. Start here.",
            "inputSchema": schema(json!({}), &[]),
        }),
        json!({
            "name": "list_items",
            "description": "All work items as JSON (id, number, title, step, status, detail, iteration, runs, pr_url).",
            "inputSchema": schema(json!({}), &[]),
        }),
        json!({
            "name": "list_steps",
            "description": "The pipeline steps in order (id, name, model, workers, approval, allowed targets).",
            "inputSchema": schema(json!({}), &[]),
        }),
        json!({
            "name": "get_item",
            "description": "Details of one work item: status, all runs, and the message of its latest finished run.",
            "inputSchema": schema(json!({"id": id}), &["id"]),
        }),
        json!({
            "name": "get_run_message",
            "description": "The message an agent wrote at the end of a run.",
            "inputSchema": schema(json!({"id": id, "run": {"type": "integer", "description": "Run number"}}), &["id", "run"]),
        }),
        json!({
            "name": "add_item",
            "description": "Add a work item: a GitHub issue URL, `#123`, or plain text describing the task.",
            "inputSchema": schema(json!({"text": {"type": "string"}}), &["text"]),
        }),
        json!({
            "name": "approve",
            "description": "Approve an item that is waiting for approval to run its current step.",
            "inputSchema": schema(json!({"id": id}), &["id"]),
        }),
        json!({
            "name": "move_item",
            "description": "Move an item (not running) to a step id or \"done\", optionally with a note for the next agent.",
            "inputSchema": schema(
                json!({"id": id, "step": {"type": "string"}, "note": {"type": "string"}}),
                &["id", "step"],
            ),
        }),
        json!({
            "name": "retry",
            "description": "Queue an item (not running) to run its current step again.",
            "inputSchema": schema(json!({"id": id}), &["id"]),
        }),
        json!({
            "name": "stop",
            "description": "Stop the running agent of an item.",
            "inputSchema": schema(json!({"id": id}), &["id"]),
        }),
        json!({
            "name": "send_message",
            "description": "Send a message to an item's agent, as the human. mode: auto (default; live if running, \
                else continue the run's session, else keep as note), live, continue, or note.",
            "inputSchema": schema(
                json!({
                    "id": id,
                    "text": {"type": "string"},
                    "mode": {"type": "string", "enum": ["auto", "live", "continue", "note"]},
                    "run": {"type": "integer", "description": "Run to continue (default: latest)"},
                }),
                &["id", "text"],
            ),
        }),
    ]
}

/// Runs a tool. Failures are reported in the result (`isError`), as MCP wants.
async fn call_tool(ws: &Workspace, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let (text, is_error) = match run_tool(ws, name, &args).await {
        Ok(text) => (text, false),
        Err(e) => (format!("{e:#}"), true),
    };
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| anyhow!("missing string argument '{key}'"))
}

fn opt_str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(String::from)
}

fn opt_u32_arg(args: &Value, key: &str) -> Result<Option<u32>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| anyhow!("argument '{key}' must be a positive integer")),
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

fn repo_label(app: &App) -> String {
    format!("{} (repo {})", app.layout.repo.display(), app.id)
}

fn open_list(ws: &Workspace) -> String {
    let repos: Vec<String> = ws.apps().iter().map(|a| repo_label(a)).collect();
    if repos.is_empty() { "none".into() } else { repos.join(", ") }
}

/// The open repository named by `arg`: its id (a recent repository with that
/// id is reopened), its path, or its directory name.
async fn find_repo(ws: &Workspace, arg: &str) -> Result<Arc<App>> {
    if let Some(app) = ws.resolve(Some(arg)).await? {
        return Ok(app);
    }
    let matches: Vec<Arc<App>> = ws
        .apps()
        .into_iter()
        .filter(|a| {
            a.layout.repo == std::path::Path::new(arg)
                || a.layout.repo.file_name().is_some_and(|n| n.to_string_lossy() == arg)
        })
        .collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => bail!("no open repository '{arg}'; open: {}", open_list(ws)),
        _ => bail!("'{arg}' matches several open repositories; use the repo id. Open: {}", open_list(ws)),
    }
}

/// The repository given by the `repo` argument, or the only open one.
async fn single_app(ws: &Workspace, args: &Value) -> Result<Arc<App>> {
    if let Some(r) = args.get("repo").and_then(Value::as_str).filter(|r| !r.is_empty()) {
        return find_repo(ws, r).await;
    }
    let mut apps = ws.apps();
    match apps.len() {
        0 => bail!("no repository is open in ClaudeChain"),
        1 => Ok(apps.remove(0)),
        _ => bail!("several repositories are open; pass 'repo' (see list_repos). Open: {}", open_list(ws)),
    }
}

/// The repositories a listing covers: the `repo` argument, else all open ones.
async fn listed_apps(ws: &Workspace, args: &Value) -> Result<Vec<Arc<App>>> {
    if let Some(r) = args.get("repo").and_then(Value::as_str).filter(|r| !r.is_empty()) {
        return Ok(vec![find_repo(ws, r).await?]);
    }
    let apps = ws.apps();
    if apps.is_empty() {
        bail!("no repository is open in ClaudeChain");
    }
    Ok(apps)
}

/// The repository holding work item `id`: the `repo` argument, else the one
/// open repository that has an item with this id.
async fn app_for_item(ws: &Workspace, args: &Value, id: &str) -> Result<Arc<App>> {
    if args.get("repo").and_then(Value::as_str).is_some_and(|r| !r.is_empty()) {
        return single_app(ws, args).await;
    }
    let mut apps: Vec<Arc<App>> =
        ws.apps().into_iter().filter(|a| a.items.lock().unwrap().contains_key(id)).collect();
    match apps.len() {
        1 => Ok(apps.remove(0)),
        0 => bail!("no item '{id}' in any open repository"),
        _ => bail!("'{id}' exists in several open repositories; pass 'repo' (see list_repos)"),
    }
}

async fn run_tool(ws: &Workspace, name: &str, args: &Value) -> Result<String> {
    let item_id = || str_arg(args, "id");
    match name {
        "list_repos" => {
            let default = ws.app(None).map(|a| a.id.clone());
            let repos: Vec<Value> = ws
                .apps()
                .iter()
                .map(|a| {
                    json!({
                        "id": a.id, "repo": a.layout.repo, "main": a.layout.main,
                        "items": a.items.lock().unwrap().len(),
                        "default": default.as_deref() == Some(a.id.as_str()),
                    })
                })
                .collect();
            Ok(pretty(&json!(repos)))
        }
        "board" => {
            let boards: Vec<String> = listed_apps(ws, args)
                .await?
                .iter()
                .map(|app| {
                    let items: Vec<Item> = app.items.lock().unwrap().values().cloned().collect();
                    board_text(&app.pipeline(), &repo_label(app), &items, Utc::now())
                })
                .collect();
            Ok(boards.join("\n\n"))
        }
        "list_items" => {
            let mut items = vec![];
            for app in listed_apps(ws, args).await? {
                for item in app.items.lock().unwrap().values() {
                    let mut v = item_summary(item);
                    v["repo"] = json!(app.id);
                    items.push(v);
                }
            }
            Ok(pretty(&json!(items)))
        }
        "list_steps" => {
            let steps: Vec<Value> = single_app(ws, args)
                .await?
                .pipeline()
                .steps
                .iter()
                .map(|s| {
                    json!({
                        "id": s.id, "name": s.name, "model": s.model, "effort": s.effort,
                        "max_workers": s.max_workers, "require_approval": s.require_approval,
                        "allowed_targets": s.allowed_targets,
                    })
                })
                .collect();
            Ok(pretty(&json!(steps)))
        }
        "get_item" => {
            let id = item_id()?;
            get_item(&*app_for_item(ws, args, id).await?, id)
        }
        "get_run_message" => {
            let id = item_id()?;
            let n = opt_u32_arg(args, "run")?.ok_or_else(|| anyhow!("missing integer argument 'run'"))?;
            let app = app_for_item(ws, args, id).await?;
            run_message(&app, id, n)?.ok_or_else(|| anyhow!("run {n} of {id} has no message (yet)"))
        }
        "add_item" => {
            let app = single_app(ws, args).await?;
            let item = app.add_item(str_arg(args, "text")?).await?;
            Ok(format!("Added {} \"{}\" at step {} in {}.", item.id, item.title, item.step, repo_label(&app)))
        }
        "approve" => {
            let id = item_id()?;
            app_for_item(ws, args, id).await?.approve(id)?;
            Ok(format!("Approved {id}."))
        }
        "move_item" => {
            let id = item_id()?;
            let step = str_arg(args, "step")?;
            app_for_item(ws, args, id).await?.move_item(id, step, opt_str_arg(args, "note"))?;
            Ok(format!("Moved {id} to {step}."))
        }
        "retry" => {
            let id = item_id()?;
            app_for_item(ws, args, id).await?.retry(id)?;
            Ok(format!("Queued {id} to retry its current step."))
        }
        "stop" => {
            let id = item_id()?;
            app_for_item(ws, args, id).await?.stop(id)?;
            Ok(format!("Stopping {id}."))
        }
        "send_message" => {
            let id = item_id()?;
            let app = app_for_item(ws, args, id).await?;
            let text = str_arg(args, "text")?.to_string();
            let mode: MessageMode = match args.get("mode") {
                None | Some(Value::Null) => MessageMode::Auto,
                Some(m) => serde_json::from_value(m.clone()).context("invalid 'mode'")?,
            };
            let delivered = app.send_message(id, opt_u32_arg(args, "run")?, mode, text)?;
            Ok(format!("Message delivered to {id} ({delivered})."))
        }
        _ => bail!("unknown tool '{name}'"),
    }
}

fn item_summary(item: &Item) -> Value {
    let last = item.runs.last().map(|r| {
        json!({
            "n": r.n, "step": r.step, "running": r.ended.is_none(), "failed": r.failed,
            "outcome": r.outcome, "summary": r.transition.as_ref().map(|t| &t.summary),
        })
    });
    json!({
        "id": item.id, "number": item.number, "title": item.title, "url": item.url,
        "step": item.step, "status": item.status, "status_detail": item.status_detail,
        "since": item.since, "iteration": item.iteration, "runs": item.runs.len(),
        "last_run": last, "pr_url": item.pr_url, "branch": item.branch,
    })
}

fn run_message(app: &App, id: &str, n: u32) -> Result<Option<String>> {
    let step = {
        let items = app.items.lock().unwrap();
        let item = items.get(id).ok_or_else(|| anyhow!("no item '{id}'"))?;
        item.run(n).ok_or_else(|| anyhow!("{id} has no run {n}"))?.step.clone()
    };
    Ok(std::fs::read_to_string(app.layout.message_file(id, n, &step)).ok())
}

fn get_item(app: &App, id: &str) -> Result<String> {
    let item = app.items.lock().unwrap().get(id).cloned().ok_or_else(|| anyhow!("no item '{id}'"))?;
    let max = app.pipeline().max_iterations;
    let mut out = String::new();
    let _ = writeln!(out, "{} {}", item.id, title_with_number(&item));
    if let Some(url) = &item.url {
        let _ = writeln!(out, "Issue: {url}");
    }
    let _ = writeln!(out, "Step: {}  Status: {}", item.step, status_text(&item));
    let _ = writeln!(out, "Iteration: {}/{max}  Branch: {}", item.iteration, item.branch);
    if let Some(pr) = &item.pr_url {
        let _ = writeln!(out, "PR: {pr}");
    }
    if !item.pending_notes.is_empty() {
        let _ = writeln!(out, "Pending notes for the next run: {}", item.pending_notes.len());
    }
    let _ = writeln!(out, "\nRuns:");
    if item.runs.is_empty() {
        let _ = writeln!(out, "  (none yet)");
    }
    for r in &item.runs {
        let cost = r.cost_usd.map(|c| format!(" ${c:.2}")).unwrap_or_default();
        let _ = writeln!(out, "  #{} {} ({}){cost}: {}", r.n, r.step, r.model, run_result(r));
    }
    if let Some(r) = item.runs.iter().rev().find(|r| r.ended.is_some())
        && let Ok(Some(msg)) = run_message(app, id, r.n)
    {
        let _ = writeln!(out, "\nMessage of run #{} ({}):\n\n{}", r.n, r.step, msg.trim_end());
    }
    Ok(out)
}

fn title_with_number(item: &Item) -> String {
    match item.number {
        Some(n) => format!("#{n} \"{}\"", item.title),
        None => format!("\"{}\"", item.title),
    }
}

fn status_text(item: &Item) -> String {
    let s = match item.status {
        Status::Queued => "queued",
        Status::AwaitingApproval => "AWAITING APPROVAL",
        Status::Running => "running",
        Status::NeedsHuman => "NEEDS HUMAN",
        Status::Done => "done",
    };
    match item.status_detail.as_deref().filter(|d| !d.trim().is_empty()) {
        Some(d) => format!("{s} ({})", d.trim()),
        None => s.to_string(),
    }
}

fn run_result(r: &crate::store::Run) -> String {
    if r.ended.is_none() {
        return "running".into();
    }
    let text = r
        .outcome
        .clone()
        .or_else(|| r.transition.as_ref().map(|t| format!("→ {}: {}", t.to, t.summary)))
        .unwrap_or_else(|| "ended".into());
    if r.failed { format!("FAILED: {text}") } else { text }
}

fn age(since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - since).num_seconds().max(0);
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// The board as a list: items grouped by pipeline step (in pipeline order),
/// then done items, with the information a human needs to act.
pub fn board_text(pipeline: &Pipeline, repo: &str, items: &[Item], now: DateTime<Utc>) -> String {
    let mut out = String::new();
    let count = |s: Status| items.iter().filter(|i| i.status == s).count();
    let _ = writeln!(out, "ClaudeChain board: {repo}");
    let _ = writeln!(
        out,
        "{} items: {} running, {} queued, {} awaiting approval, {} need a human, {} done",
        items.len(),
        count(Status::Running),
        count(Status::Queued),
        count(Status::AwaitingApproval),
        count(Status::NeedsHuman),
        count(Status::Done),
    );

    let mut sorted: Vec<&Item> = items.iter().collect();
    sorted.sort_by_key(|i| i.since);
    let line = |out: &mut String, item: &Item| {
        let mut parts = vec![status_text(item), format!("for {}", age(item.since, now))];
        if item.iteration > 0 {
            parts.push(format!("iteration {}/{}", item.iteration, pipeline.max_iterations));
        }
        parts.push(match item.runs.len() {
            1 => "1 run".into(),
            n => format!("{n} runs"),
        });
        let _ = writeln!(out, "- {} {}: {}", item.id, title_with_number(item), parts.join(", "));
        if let Some(r) = item.runs.last() {
            let _ = writeln!(out, "    last run #{} {}: {}", r.n, r.step, run_result(r));
        }
        if let Some(pr) = &item.pr_url {
            let _ = writeln!(out, "    PR: {pr}");
        }
    };
    let section = |out: &mut String, title: String, group: &[&Item]| {
        let _ = writeln!(out, "\n## {title} ({})", group.len());
        if group.is_empty() {
            let _ = writeln!(out, "  (empty)");
        }
        for item in group {
            line(out, item);
        }
    };

    for step in &pipeline.steps {
        let group: Vec<&Item> = sorted.iter().copied().filter(|i| i.status != Status::Done && i.step == step.id).collect();
        section(&mut out, format!("{} [{}]", step.name, step.id), &group);
    }
    let orphans: Vec<&Item> = sorted
        .iter()
        .copied()
        .filter(|i| i.status != Status::Done && pipeline.step(&i.step).is_none())
        .collect();
    if !orphans.is_empty() {
        section(&mut out, "Unknown step".into(), &orphans);
    }
    let done: Vec<&Item> = sorted.iter().copied().filter(|i| i.status == Status::Done).collect();
    section(&mut out, "Done".into(), &done);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipeline() -> Pipeline {
        serde_json::from_value(json!({"step": [
            {"id": "triage", "name": "Triage", "model": "m", "allowed_targets": ["next"], "tools": []},
            {"id": "review", "name": "Review", "model": "m", "allowed_targets": ["next"], "tools": []},
        ]}))
        .unwrap()
    }

    fn item(id: &str, step: &str, status: &str, extra: Value) -> Item {
        let mut v = json!({
            "id": id, "title": format!("Title {id}"), "branch": format!("claudechain/{id}"),
            "created": "2026-11-14T10:00:00Z", "since": "2026-11-14T11:00:00Z",
            "step": step, "status": status,
        });
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn board_groups_by_step() {
        let now: DateTime<Utc> = "2026-11-14T11:05:00Z".parse().unwrap();
        let items = vec![
            item("item-1", "review", "needs_human", json!({"number": 12, "status_detail": "unclear spec", "iteration": 2})),
            item("item-2", "triage", "running", json!({"runs": [
                {"n": 1, "step": "triage", "model": "m", "started": "2026-11-14T11:00:00Z"}
            ]})),
            item("item-3", "review", "done", json!({"pr_url": "https://example.com/pr/1"})),
            item("item-4", "gone", "queued", json!({})),
        ];
        let text = board_text(&pipeline(), "/repo", &items, now);
        assert!(text.contains("4 items: 1 running, 1 queued, 0 awaiting approval, 1 need a human, 1 done"), "{text}");
        let triage = text.find("## Triage [triage] (1)").unwrap();
        let review = text.find("## Review [review] (1)").unwrap();
        let unknown = text.find("## Unknown step (1)").unwrap();
        let done = text.find("## Done (1)").unwrap();
        assert!(triage < review && review < unknown && unknown < done, "{text}");
        assert!(text.contains("- item-1 #12 \"Title item-1\": NEEDS HUMAN (unclear spec), for 5m, iteration 2/6, 0 runs"), "{text}");
        assert!(text.contains("last run #1 triage: running"), "{text}");
        assert!(text.contains("PR: https://example.com/pr/1"), "{text}");
        let item3 = text.find("- item-3").unwrap();
        assert!(item3 > done, "{text}");
    }

    #[test]
    fn age_units() {
        let now: DateTime<Utc> = "2026-11-14T12:00:00Z".parse().unwrap();
        let ago = |s| now - chrono::Duration::seconds(s);
        assert_eq!(age(ago(5), now), "5s");
        assert_eq!(age(ago(120), now), "2m");
        assert_eq!(age(ago(7200), now), "2h");
        assert_eq!(age(ago(200_000), now), "2d");
    }

    #[tokio::test]
    async fn json_rpc_dispatch() {
        let ws = Workspace::new("claude".into());
        let init = dispatch(&ws, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26"}}))
        .await
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "claudechain");

        assert!(dispatch(&ws, json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await.is_none());

        let list = dispatch(&ws, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).await.unwrap();
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"board") && names.contains(&"approve") && names.contains(&"list_repos"), "{names:?}");
        let board = list["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "board").unwrap();
        assert!(board["inputSchema"]["properties"]["repo"].is_object(), "{board}");

        let unknown = dispatch(&ws, json!({"jsonrpc": "2.0", "id": 3, "method": "nope"})).await.unwrap();
        assert_eq!(unknown["error"]["code"], -32601);

        // No repository open: the tool reports an error instead of failing the request.
        let call = dispatch(&ws, json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "board", "arguments": {}}}))
        .await
        .unwrap();
        assert_eq!(call["result"]["isError"], true);
    }
}

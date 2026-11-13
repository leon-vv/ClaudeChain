use std::convert::Infallible;
use std::path::{Component, Path as FsPath};
use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::config;
use crate::git;
use crate::orchestrator::{self, App, MessageMode};
use crate::store::Layout;
use crate::workspace::{self, Workspace};

type Ws = Arc<Workspace>;

/// The app for the currently open repository; fails when none is open.
pub struct Cur(Arc<App>);

impl FromRequestParts<Ws> for Cur {
    type Rejection = ApiError;
    async fn from_request_parts(_: &mut Parts, ws: &Ws) -> Result<Self, ApiError> {
        ws.app().map(Cur).ok_or_else(|| anyhow::anyhow!("no repository is open").into())
    }
}

pub struct ApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, Json(json!({"error": format!("{:#}", self.0)}))).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

pub fn router(ws: Ws, allowed_hosts: Vec<String>) -> Router {
    Router::new()
        .route("/", get(|| static_file("text/html; charset=utf-8", include_str!("../static/index.html"))))
        .route("/app.js", get(|| static_file("text/javascript", include_str!("../static/app.js"))))
        .route("/resources.js", get(|| static_file("text/javascript", include_str!("../static/resources.js"))))
        .route("/style.css", get(|| static_file("text/css", include_str!("../static/style.css"))))
        .route("/api/state", get(state))
        .route("/api/events", get(events))
        .route("/api/workspace", post(open_workspace))
        .route("/api/fs", get(list_dir))
        .route("/api/resources", get(resources))
        .route("/api/limits", put(put_limits))
        .route("/api/pipeline", put(put_pipeline))
        .route("/api/settings", put(put_settings))
        .route("/api/steps/{step}", put(put_step))
        .route("/api/prompts/{id}", get(get_prompt).put(put_prompt))
        .route("/api/items", post(add_item))
        .route("/api/items/{id}", axum::routing::delete(remove_item))
        .route("/api/items/{id}/approve", post(approve))
        .route("/api/items/{id}/move", post(move_item))
        .route("/api/items/{id}/retry", post(retry))
        .route("/api/items/{id}/restart", post(restart))
        .route("/api/items/{id}/stop", post(stop))
        .route("/api/items/{id}/message", post(message))
        .route("/api/items/{id}/runs/{n}", get(get_run))
        .route("/api/items/{id}/files", get(list_files))
        .route("/api/items/{id}/file", get(get_file))
        .route("/api/items/{id}/diff", get(get_diff))
        .layer(middleware::from_fn(move |req, next| guard(allowed_hosts.clone(), req, next)))
        .with_state(ws)
}

/// The API can run agents and read the filesystem, so other websites must not
/// be able to use it through the browser: the Host header must be a local name
/// (against DNS rebinding) and changes need a custom header, which a
/// cross-origin page can't send without a CORS preflight we never approve.
async fn guard(allowed_hosts: Vec<String>, req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let name = host.rsplit_once(':').map_or(host, |(n, port)| if port.chars().all(|c| c.is_ascii_digit()) { n } else { host });
    let host_ok = allowed_hosts.is_empty() || allowed_hosts.iter().any(|a| a == name);
    if !host_ok {
        return (StatusCode::FORBIDDEN, "unexpected Host header").into_response();
    }
    let changes = !matches!(*req.method(), Method::GET | Method::HEAD);
    if changes && req.headers().get("x-claudechain").is_none() {
        return (StatusCode::FORBIDDEN, "missing x-claudechain header").into_response();
    }
    next.run(req).await
}

async fn static_file(content_type: &'static str, body: &'static str) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache")], body)
}

async fn state(State(ws): State<Ws>) -> Json<Value> {
    let recent = ws.recent();
    let Some(app) = ws.app() else {
        return Json(json!({
            "repo": null,
            "recent": recent,
            "home": workspace::home_dir(),
            "account": orchestrator::account_json(&ws.account),
        }));
    };
    // `step` is named for TOML's [[step]] tables; the API calls it `steps`.
    let mut pipeline = json!(app.pipeline());
    if let Some(obj) = pipeline.as_object_mut() {
        let steps = obj.remove("step").unwrap_or_default();
        obj.insert("steps".into(), steps);
    }
    let items: Vec<_> = app.items.lock().unwrap().values().cloned().collect();
    Json(json!({
        "repo": app.layout.repo,
        "data": app.layout.data,
        "default_data": workspace::default_data_dir(&app.layout.repo),
        "recent": recent,
        "home": workspace::home_dir(),
        "account": orchestrator::account_json(&ws.account),
        "pipeline": pipeline,
        "items": items,
    }))
}

async fn events(State(ws): State<Ws>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(ws.events.subscribe()).map(|msg| {
        Ok(match msg {
            Ok(ev) => Event::default().json_data(ev).unwrap_or_default(),
            // The browser fell behind; it reloads the full state.
            Err(BroadcastStreamRecvError::Lagged(_)) => Event::default().data(r#"{"kind":"resync"}"#),
        })
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct OpenWorkspace {
    repo: String,
    data: Option<String>,
}

async fn open_workspace(State(ws): State<Ws>, Json(o): Json<OpenWorkspace>) -> ApiResult<Json<Value>> {
    let data = o.data.as_deref().map(str::trim).filter(|d| !d.is_empty()).map(FsPath::new);
    ws.open(FsPath::new(o.repo.trim()), data).await?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct DirQuery {
    #[serde(default)]
    path: String,
    #[serde(default)]
    hidden: bool,
}

async fn list_dir(Query(q): Query<DirQuery>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(workspace::list_dir(&q.path, q.hidden)?)))
}

/// System load, history, and CPU/memory of each running agent's process tree.
async fn resources(State(ws): State<Ws>) -> Json<Value> {
    let agents: Vec<Value> = ws
        .app()
        .map(|app| app.agents())
        .unwrap_or_default()
        .into_iter()
        .map(|a| {
            let usage = ws.monitor.tree(a.pid);
            json!({"agent": a, "usage": usage})
        })
        .collect();
    Json(json!({
        "system": ws.monitor.latest(),
        "history": ws.monitor.history(),
        "agents": agents,
        "server": ws.monitor.tree(std::process::id()),
    }))
}

#[derive(Deserialize)]
struct LimitSettings {
    pause_above: Option<f64>,
}

async fn put_limits(State(ws): State<Ws>, Json(l): Json<LimitSettings>) -> ApiResult<Json<Value>> {
    ws.account.set_pause_above(l.pause_above);
    let account = orchestrator::account_json(&ws.account);
    let _ = ws.events.send(orchestrator::UiEvent::Limits { account: account.clone() });
    Ok(Json(account))
}

async fn put_pipeline(Cur(app): Cur, Json(p): Json<config::Pipeline>) -> ApiResult<Json<Value>> {
    app.replace_pipeline(p)?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct Settings {
    max_iterations: u32,
}

async fn put_settings(Cur(app): Cur, Json(s): Json<Settings>) -> ApiResult<Json<Value>> {
    app.update_pipeline(|p| {
        p.max_iterations = s.max_iterations;
        Ok(())
    })?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
#[derive(Default)]
struct StepUpdate {
    name: Option<String>,
    model: Option<String>,
    /// Thinking level; an empty string resets it to Claude's default.
    effort: Option<String>,
    max_workers: Option<usize>,
    require_approval: Option<bool>,
}

/// Applies the fields present in `u` to `s`. Fails on an empty name.
fn apply_step_update(s: &mut config::Step, u: StepUpdate) -> anyhow::Result<()> {
    if let Some(n) = u.name {
        let n = n.trim();
        if n.is_empty() {
            anyhow::bail!("step name cannot be empty");
        }
        s.name = n.to_string();
    }
    if let Some(m) = u.model {
        s.model = m;
    }
    if let Some(e) = u.effort {
        let e = e.trim();
        if e.is_empty() {
            s.effort = None;
        } else if config::EFFORT_LEVELS.contains(&e) {
            s.effort = Some(e.to_string());
        } else {
            anyhow::bail!("unknown effort '{e}': use one of {}", config::EFFORT_LEVELS.join(", "));
        }
    }
    if let Some(w) = u.max_workers {
        s.max_workers = w;
    }
    if let Some(a) = u.require_approval {
        s.require_approval = a;
    }
    Ok(())
}

async fn put_step(Cur(app): Cur, Path(step): Path<String>, Json(u): Json<StepUpdate>) -> ApiResult<Json<Value>> {
    app.update_pipeline(|p| {
        let s = p.step_mut(&step).ok_or_else(|| anyhow::anyhow!("unknown step '{step}'"))?;
        apply_step_update(s, u)
    })?;
    Ok(Json(json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step() -> config::Step {
        config::Step {
            id: "review".into(),
            name: "Review".into(),
            model: "claude-opus-5-5".into(),
            effort: None,
            max_workers: 1,
            require_approval: false,
            allowed_targets: vec!["next".into()],
            tools: vec!["Write".into()],
            permission_mode: "dontAsk".into(),
        }
    }

    fn update(name: Option<&str>) -> StepUpdate {
        StepUpdate { name: name.map(Into::into), ..Default::default() }
    }

    #[test]
    fn rename_trims_and_keeps_id() {
        let mut s = step();
        apply_step_update(&mut s, update(Some("  Code review "))).unwrap();
        assert_eq!(s.name, "Code review");
        assert_eq!(s.id, "review");
        assert_eq!(s.model, "claude-opus-5-5");
    }

    #[test]
    fn rename_rejects_empty() {
        let mut s = step();
        assert!(apply_step_update(&mut s, update(Some("   "))).is_err());
        assert_eq!(s.name, "Review");
    }

    #[test]
    fn update_without_name_leaves_name() {
        let mut s = step();
        apply_step_update(&mut s, StepUpdate { max_workers: Some(3), ..Default::default() }).unwrap();
        assert_eq!(s.name, "Review");
        assert_eq!(s.max_workers, 3);
    }

    #[test]
    fn effort_set_clear_and_reject() {
        let mut s = step();
        apply_step_update(&mut s, StepUpdate { effort: Some("high".into()), ..Default::default() }).unwrap();
        assert_eq!(s.effort.as_deref(), Some("high"));
        assert!(apply_step_update(&mut s, StepUpdate { effort: Some("extreme".into()), ..Default::default() }).is_err());
        assert_eq!(s.effort.as_deref(), Some("high"));
        apply_step_update(&mut s, StepUpdate { effort: Some(String::new()), ..Default::default() }).unwrap();
        assert_eq!(s.effort, None);
    }
}

fn check_prompt_id(app: &App, id: &str) -> ApiResult<()> {
    if id != "general" && app.pipeline().step(id).is_none() {
        return Err(anyhow::anyhow!("unknown prompt '{id}'").into());
    }
    Ok(())
}

async fn get_prompt(Cur(app): Cur, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    check_prompt_id(&app, &id)?;
    Ok(Json(json!({"text": config::read_prompt(&app.layout.data, &id)})))
}

#[derive(Deserialize)]
struct Text {
    text: String,
}

async fn put_prompt(Cur(app): Cur, Path(id): Path<String>, Json(t): Json<Text>) -> ApiResult<Json<Value>> {
    check_prompt_id(&app, &id)?;
    config::write_prompt(&app.layout.data, &id, &t.text)?;
    Ok(Json(json!({})))
}

async fn add_item(Cur(app): Cur, Json(t): Json<Text>) -> ApiResult<Json<Value>> {
    let item = app.add_item(&t.text).await?;
    Ok(Json(json!(item)))
}

async fn remove_item(Cur(app): Cur, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    app.remove_item(&id).await?;
    Ok(Json(json!({})))
}

async fn approve(Cur(app): Cur, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    app.approve(&id)?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct Move {
    step: String,
    note: Option<String>,
}

async fn move_item(Cur(app): Cur, Path(id): Path<String>, Json(m): Json<Move>) -> ApiResult<Json<Value>> {
    app.move_item(&id, &m.step, m.note)?;
    Ok(Json(json!({})))
}

async fn retry(Cur(app): Cur, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    app.retry(&id)?;
    Ok(Json(json!({})))
}

async fn restart(Cur(app): Cur, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    app.restart_item(&id).await?;
    Ok(Json(json!({})))
}

async fn stop(Cur(app): Cur, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    app.stop(&id)?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct Message {
    text: String,
    mode: MessageMode,
    run: Option<u32>,
}

async fn message(Cur(app): Cur, Path(id): Path<String>, Json(m): Json<Message>) -> ApiResult<Json<Value>> {
    app.send_message(&id, m.run, m.mode, m.text)?;
    Ok(Json(json!({})))
}

fn read_opt(path: &FsPath) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

async fn get_run(Cur(app): Cur, Path((id, n)): Path<(String, u32)>) -> ApiResult<Json<Value>> {
    let run = {
        let items = app.items.lock().unwrap();
        let item = items.get(&id).ok_or_else(|| anyhow::anyhow!("no item '{id}'"))?;
        item.run(n).cloned().ok_or_else(|| anyhow::anyhow!("no run {n}"))?
    };
    let l: &Layout = &app.layout;
    let dir = l.run_dir(&id, n, &run.step);
    let input: Value = read_opt(&dir.join("input.json"))
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let events: Vec<Value> = read_opt(&dir.join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    Ok(Json(json!({
        "run": run,
        "input": input,
        "events": events,
        "message": read_opt(&l.message_file(&id, n, &run.step)),
        "transition": read_opt(&l.transition_file(&id, n, &run.step)),
        "stderr": read_opt(&dir.join("stderr.log")).filter(|s| !s.trim().is_empty()),
    })))
}

async fn list_files(Cur(app): Cur, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let root = app.layout.item_dir(&id);
    let mut files = vec![];
    collect_files(&root, &root, &mut files)?;
    files.sort();
    Ok(Json(json!(files)))
}

fn collect_files(root: &FsPath, dir: &FsPath, out: &mut Vec<String>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.display().to_string());
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}

async fn get_file(Cur(app): Cur, Path(id): Path<String>, Query(q): Query<FileQuery>) -> ApiResult<Json<Value>> {
    let rel = FsPath::new(&q.path);
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(anyhow::anyhow!("invalid path").into());
    }
    let text = std::fs::read_to_string(app.layout.item_dir(&id).join(rel))?;
    Ok(Json(json!({"text": text})))
}

async fn get_diff(Cur(app): Cur, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let base = app.items.lock().unwrap().get(&id).and_then(|i| i.base_commit.clone());
    let text = git::diff(&app.layout.worktree(&id), base.as_deref()).await?;
    Ok(Json(json!({"text": text})))
}

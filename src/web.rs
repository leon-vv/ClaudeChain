use std::convert::Infallible;
use std::path::{Component, Path as FsPath};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
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
use crate::orchestrator::{App, MessageMode};
use crate::store::Layout;

type AppState = Arc<App>;

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

pub fn router(app: AppState) -> Router {
    Router::new()
        .route("/", get(|| static_file("text/html; charset=utf-8", include_str!("../static/index.html"))))
        .route("/app.js", get(|| static_file("text/javascript", include_str!("../static/app.js"))))
        .route("/style.css", get(|| static_file("text/css", include_str!("../static/style.css"))))
        .route("/api/state", get(state))
        .route("/api/events", get(events))
        .route("/api/settings", put(put_settings))
        .route("/api/steps/{step}", put(put_step))
        .route("/api/prompts/{id}", get(get_prompt).put(put_prompt))
        .route("/api/items", post(add_item))
        .route("/api/items/{id}", axum::routing::delete(remove_item))
        .route("/api/items/{id}/approve", post(approve))
        .route("/api/items/{id}/move", post(move_item))
        .route("/api/items/{id}/retry", post(retry))
        .route("/api/items/{id}/stop", post(stop))
        .route("/api/items/{id}/message", post(message))
        .route("/api/items/{id}/runs/{n}", get(get_run))
        .route("/api/items/{id}/files", get(list_files))
        .route("/api/items/{id}/file", get(get_file))
        .route("/api/items/{id}/diff", get(get_diff))
        .with_state(app)
}

async fn static_file(content_type: &'static str, body: &'static str) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache")], body)
}

async fn state(State(app): State<AppState>) -> Json<Value> {
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
        "pipeline": pipeline,
        "items": items,
    }))
}

async fn events(State(app): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(app.events.subscribe()).map(|msg| {
        Ok(match msg {
            Ok(ev) => Event::default().json_data(ev).unwrap_or_default(),
            // The browser fell behind; it reloads the full state.
            Err(BroadcastStreamRecvError::Lagged(_)) => Event::default().data(r#"{"kind":"resync"}"#),
        })
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct Settings {
    max_iterations: u32,
}

async fn put_settings(State(app): State<AppState>, Json(s): Json<Settings>) -> ApiResult<Json<Value>> {
    app.update_pipeline(|p| {
        p.max_iterations = s.max_iterations;
        Ok(())
    })?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct StepUpdate {
    model: Option<String>,
    max_workers: Option<usize>,
    require_approval: Option<bool>,
}

async fn put_step(State(app): State<AppState>, Path(step): Path<String>, Json(u): Json<StepUpdate>) -> ApiResult<Json<Value>> {
    app.update_pipeline(|p| {
        let s = p.step_mut(&step).ok_or_else(|| anyhow::anyhow!("unknown step '{step}'"))?;
        if let Some(m) = u.model {
            s.model = m;
        }
        if let Some(w) = u.max_workers {
            s.max_workers = w;
        }
        if let Some(a) = u.require_approval {
            s.require_approval = a;
        }
        Ok(())
    })?;
    Ok(Json(json!({})))
}

fn check_prompt_id(app: &App, id: &str) -> ApiResult<()> {
    if id != "general" && app.pipeline().step(id).is_none() {
        return Err(anyhow::anyhow!("unknown prompt '{id}'").into());
    }
    Ok(())
}

async fn get_prompt(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    check_prompt_id(&app, &id)?;
    Ok(Json(json!({"text": config::read_prompt(&app.layout.data, &id)})))
}

#[derive(Deserialize)]
struct Text {
    text: String,
}

async fn put_prompt(State(app): State<AppState>, Path(id): Path<String>, Json(t): Json<Text>) -> ApiResult<Json<Value>> {
    check_prompt_id(&app, &id)?;
    config::write_prompt(&app.layout.data, &id, &t.text)?;
    Ok(Json(json!({})))
}

async fn add_item(State(app): State<AppState>, Json(t): Json<Text>) -> ApiResult<Json<Value>> {
    let item = app.add_item(&t.text).await?;
    Ok(Json(json!(item)))
}

async fn remove_item(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    app.remove_item(&id).await?;
    Ok(Json(json!({})))
}

async fn approve(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    app.approve(&id)?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct Move {
    step: String,
    note: Option<String>,
}

async fn move_item(State(app): State<AppState>, Path(id): Path<String>, Json(m): Json<Move>) -> ApiResult<Json<Value>> {
    app.move_item(&id, &m.step, m.note)?;
    Ok(Json(json!({})))
}

async fn retry(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    app.retry(&id)?;
    Ok(Json(json!({})))
}

async fn stop(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    app.stop(&id)?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct Message {
    text: String,
    mode: MessageMode,
    run: Option<u32>,
}

async fn message(State(app): State<AppState>, Path(id): Path<String>, Json(m): Json<Message>) -> ApiResult<Json<Value>> {
    app.send_message(&id, m.run, m.mode, m.text)?;
    Ok(Json(json!({})))
}

fn read_opt(path: &FsPath) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

async fn get_run(State(app): State<AppState>, Path((id, n)): Path<(String, u32)>) -> ApiResult<Json<Value>> {
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

async fn list_files(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
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

async fn get_file(State(app): State<AppState>, Path(id): Path<String>, Query(q): Query<FileQuery>) -> ApiResult<Json<Value>> {
    let rel = FsPath::new(&q.path);
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(anyhow::anyhow!("invalid path").into());
    }
    let text = std::fs::read_to_string(app.layout.item_dir(&id).join(rel))?;
    Ok(Json(json!({"text": text})))
}

async fn get_diff(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let base = app.items.lock().unwrap().get(&id).and_then(|i| i.base_commit.clone());
    let text = git::diff(&app.layout.worktree(&id), base.as_deref()).await?;
    Ok(Json(json!({"text": text})))
}

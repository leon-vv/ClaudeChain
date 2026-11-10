mod account;
mod config;
mod git;
mod orchestrator;
mod runner;
mod store;
mod sysmon;
mod web;
mod workspace;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use crate::workspace::Workspace;

/// Web UI that runs GitHub issues through a pipeline of Claude agents.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// The git repository the agents work on. Without it, the most recently
    /// used one is opened, or you pick one in the web UI.
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Where pipeline config, prompts, work items and worktrees are stored
    /// (default: <repo>/.claudechain).
    #[arg(long)]
    data: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 7878)]
    port: u16,
    /// The Claude Code CLI to run agents with.
    #[arg(long, default_value = "claude")]
    claude: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "claudechain=info".into()),
        )
        .init();
    let args = Args::parse();

    let ws = Workspace::new(args.claude);
    tokio::spawn(ws.clone().monitor_loop());
    let initial = match args.repo {
        Some(repo) => Some((repo, args.data)),
        None => ws.recent().into_iter().next().map(|r| (r.repo, Some(r.data))),
    };
    if let Some((repo, data)) = initial
        && let Err(e) = ws.open(&repo, data.as_deref()).await {
            tracing::warn!("could not open {}: {e:#}; pick a repository in the web UI", repo.display());
        }

    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("binding {addr}"))?;
    // Names the browser may use to reach us (see web::guard). Binding to all
    // interfaces means the user opted into remote access, so allow any.
    let allowed_hosts = match args.host.as_str() {
        "0.0.0.0" | "::" | "[::]" => vec![],
        h => ["localhost", "127.0.0.1", "[::1]", "::1", h].map(String::from).to_vec(),
    };
    tracing::info!("open http://{addr}");
    // No graceful shutdown: open SSE streams would keep it waiting forever.
    // Leaving main drops all tasks, which kills running agents (kill_on_drop);
    // their items are requeued on the next start.
    tokio::select! {
        res = axum::serve(listener, web::router(ws, allowed_hosts)) => res?,
        _ = shutdown_signal() => {}
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
    tracing::info!("shutting down");
}

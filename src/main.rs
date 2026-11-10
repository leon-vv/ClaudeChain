mod config;
mod git;
mod orchestrator;
mod runner;
mod store;
mod web;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use crate::orchestrator::App;
use crate::store::Layout;

/// Web UI that runs GitHub issues through a pipeline of Claude agents.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// The git repository the agents work on.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
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

    let repo = std::fs::canonicalize(&args.repo).with_context(|| format!("repo {}", args.repo.display()))?;
    git::check_repo(&repo).await.with_context(|| format!("{} is not a git repository", repo.display()))?;
    let data = args.data.unwrap_or_else(|| repo.join(".claudechain"));
    std::fs::create_dir_all(&data)?;
    let data = std::fs::canonicalize(&data)?;
    git::exclude_path(&repo, &data).await?;

    let app = App::new(Layout { repo: repo.clone(), data: data.clone() }, args.claude)?;
    tokio::spawn(app.clone().scheduler());

    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("binding {addr}"))?;
    tracing::info!("repo {}", repo.display());
    tracing::info!("data {}", data.display());
    tracing::info!("open http://{addr}");
    // No graceful shutdown: open SSE streams would keep it waiting forever.
    // Leaving main drops all tasks, which kills running agents (kill_on_drop);
    // their items are requeued on the next start.
    tokio::select! {
        res = axum::serve(listener, web::router(app)) => res?,
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

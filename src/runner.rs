//! Runs one agent as a `claude -p` child process speaking stream-json on
//! stdin and stdout.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

pub struct RunSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub initial_prompt: String,
    pub events_path: PathBuf,
    pub stderr_path: PathBuf,
    /// Set to the child's process id once it has started.
    pub pid: Arc<AtomicU32>,
}

/// Handle the orchestrator keeps for a running agent.
pub struct RunControl {
    pub messages: mpsc::UnboundedSender<String>,
    pub stop: Option<oneshot::Sender<()>>,
    pub pid: Arc<AtomicU32>,
}

#[derive(Debug, Default)]
pub struct RunResult {
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
    pub result_text: Option<String>,
    pub is_error: bool,
    pub stopped: bool,
    pub exit: Option<String>,
    /// Human messages that arrived after the agent finished its turn.
    pub undelivered: Vec<String>,
}

pub fn user_message(text: &str) -> String {
    json!({"type": "user", "message": {"role": "user", "content": text}}).to_string()
}

/// Our own entries in events.jsonl, next to the CLI's events.
pub fn local_event(subtype: &str, text: &str) -> Value {
    json!({"type": "claudechain", "subtype": subtype, "text": text, "at": chrono::Utc::now()})
}

/// Environment variables that would make the CLI bill an API key or talk to
/// a different endpoint instead of using the subscription login, and the ones
/// `cargo run` sets for ClaudeChain itself. rustup's RUSTUP_TOOLCHAIN would
/// otherwise override the worktree's rust-toolchain.toml in every build the
/// agent runs, so its builds use a different compiler and share no cache.
fn scrub_env(cmd: &mut Command) {
    for (key, _) in std::env::vars_os() {
        let k = key.to_string_lossy();
        if k.starts_with("ANTHROPIC_")
            || k == "CLAUDECODE"
            || k.starts_with("CLAUDE_CODE_")
            || from_cargo_run(&k)
        {
            cmd.env_remove(&key);
        }
    }
}

/// Set by rustup's proxy or by `cargo run` for the program being run; never
/// configuration the user chose (CARGO_HOME, CARGO_TARGET_DIR stay).
fn from_cargo_run(k: &str) -> bool {
    matches!(
        k,
        "RUSTUP_TOOLCHAIN"
            | "RUSTUP_TOOLCHAIN_SOURCE"
            | "CARGO"
            | "CARGO_MANIFEST_DIR"
            | "CARGO_MANIFEST_PATH"
            | "CARGO_CRATE_NAME"
            | "CARGO_BIN_NAME"
            | "CARGO_PRIMARY_PACKAGE"
    ) || k.starts_with("CARGO_PKG_")
}

/// Runs the agent to completion. `on_event` sees every event (CLI events and
/// local ones) as it is appended to events.jsonl.
pub async fn run(
    spec: RunSpec,
    mut messages: mpsc::UnboundedReceiver<String>,
    mut stop: oneshot::Receiver<()>,
    mut on_event: impl FnMut(&Value),
) -> Result<RunResult> {
    let mut events = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.events_path)
        .await
        .with_context(|| format!("opening {}", spec.events_path.display()))?;
    let stderr_file = std::fs::File::create(&spec.stderr_path)?;

    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_file))
        .kill_on_drop(true);
    scrub_env(&mut cmd);
    // If ClaudeChain dies (even by SIGKILL), take the agent down with it.
    #[cfg(target_os = "linux")]
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().with_context(|| format!("starting {}", spec.program))?;
    spec.pid.store(child.id().unwrap_or(0), Ordering::Relaxed);
    let mut stdin = child.stdin.take();
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout")).lines();

    let mut log = async |events: &mut tokio::fs::File, line: &str, value: &Value| {
        let _ = events.write_all(line.as_bytes()).await;
        let _ = events.write_all(b"\n").await;
        on_event(value);
    };

    if let Some(w) = stdin.as_mut() {
        w.write_all(user_message(&spec.initial_prompt).as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
    }

    let mut result = RunResult::default();
    let mut messages_open = true;
    loop {
        tokio::select! {
            line = stdout.next_line() => {
                let Some(line) = line? else { break };
                let value: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|_| local_event("raw_output", &line));
                if result.session_id.is_none()
                    && let Some(id) = value.get("session_id").and_then(Value::as_str) {
                        result.session_id = Some(id.to_string());
                    }
                if value["type"] == "result" {
                    result.cost_usd = value["total_cost_usd"].as_f64();
                    result.result_text = value["result"].as_str().map(String::from);
                    result.is_error = value["is_error"].as_bool().unwrap_or(false);
                    // The turn is over: closing stdin lets the CLI exit.
                    // Messages arriving after this need a resumed session.
                    stdin = None;
                }
                log(&mut events, &line, &value).await;
            }
            msg = messages.recv(), if messages_open => {
                match msg {
                    Some(text) => {
                        let Some(w) = stdin.as_mut() else {
                            let ev = local_event("human_message_dropped", &format!(
                                "The agent already finished its turn; this message is kept for the next run: {text}"));
                            log(&mut events, &ev.to_string(), &ev).await;
                            result.undelivered.push(text);
                            continue;
                        };
                        let line = user_message(&format!("Message from the human supervising this pipeline:\n\n{text}"));
                        if w.write_all(format!("{line}\n").as_bytes()).await.is_ok() {
                            let _ = w.flush().await;
                            let ev = local_event("human_message", &text);
                            log(&mut events, &ev.to_string(), &ev).await;
                        }
                    }
                    None => messages_open = false,
                }
            }
            _ = &mut stop, if !result.stopped => {
                result.stopped = true;
                let ev = local_event("stopped", "Stopped by the human");
                log(&mut events, &ev.to_string(), &ev).await;
                let _ = child.start_kill();
            }
        }
    }
    let status = child.wait().await?;
    if !status.success() {
        result.exit = Some(status.to_string());
    }
    Ok(result)
}

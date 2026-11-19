//! "Generate pipeline using prompt": asks Claude for a pipeline (steps,
//! workers, permissions, models and a prompt per step) from a short
//! description. The result goes back to the pipeline editor as a draft; the
//! user reviews it and saves it through the normal `PUT /api/pipeline`.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::{EFFORT_LEVELS, Pipeline, Step};

/// Tools the generated steps may use (the ones the editor knows about).
pub const KNOWN_TOOLS: &[&str] =
    &["Read", "Grep", "Glob", "Edit", "Write", "Bash", "WebFetch", "WebSearch", "NotebookEdit", "TodoWrite", "Agent"];
pub const PERMISSION_MODES: &[&str] = &["dontAsk", "acceptEdits", "bypassPermissions", "auto", "plan"];
pub const MAX_WORKERS: usize = 32;
const TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedStep {
    #[serde(flatten)]
    pub step: Step,
    /// The step's prompt (`prompts/<id>.md`), written by the UI after saving.
    #[serde(default)]
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Generated {
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    pub steps: Vec<GeneratedStep>,
}

fn default_max_iterations() -> u32 {
    6
}

/// Instructions describing the JSON the model must return.
pub fn system_prompt(models: &[String]) -> String {
    format!(
        r#"You design agent pipelines for ClaudeChain. ClaudeChain runs a chain of Claude Code agents on work items (GitHub issues or pasted tasks) for one git repository. Each step is one agent run in a git worktree of the repository; it reads the issue and the messages of earlier steps, does its job, writes a message file for the next agent and a transition file, and hands the item on ("next"), sends it back to an earlier step, or stops and asks the human ("human"). A shared general prompt already explains the workspace, message and transition files, so step prompts only describe the step's own job.

The user describes the pipeline they want. Answer with ONLY one JSON object, no prose and no code fences, of this shape:

{{
  "max_iterations": 6,
  "steps": [
    {{
      "id": "triage",
      "name": "Triage & Context",
      "model": "claude-haiku-5-5",
      "effort": null,
      "max_workers": 2,
      "require_approval": false,
      "allowed_targets": ["next", "human"],
      "tools": ["Read", "Grep", "Glob", "Bash", "Write"],
      "permission_mode": "dontAsk",
      "prompt": "Your step is **Triage & Context**.\n\n..."
    }}
  ]
}}

Rules:
- Steps run in order; usually 3 to 7 steps. The last step's "next" finishes the item.
- "id": unique, lowercase letters, digits, '_' or '-'; not general, next, human or done.
- "name": short human-readable name.
- "model": one of {models}. Use cheaper models for mechanical steps (triage, building, testing, publishing) and the strongest model for design and implementation work.
- "effort": null (default) or one of {efforts}.
- "max_workers": 0 to {max_workers}: how many items this step may work on at the same time.
- "require_approval": true when a human should approve before the step starts (e.g. publishing, ordering parts, anything outward-facing or irreversible).
- "allowed_targets": non-empty; "next", "human" and/or ids of other steps in this pipeline (e.g. a test step may send work back to the implementation step). Include "human" in every step.
- "tools": from {tools}. Every step needs "Write". Only steps that change files need "Edit"; only steps that need the web get WebFetch/WebSearch.
- "permission_mode": one of {modes}. Use "dontAsk" unless the user asks otherwise; never use "bypassPermissions" unless the user explicitly asks for it.
- "max_iterations": 0 to 100, how often an item may be sent backwards in total.
- "prompt": the step's instructions in Markdown (roughly 5 to 25 lines): what the agent does, what it must not do, what to put in its message for the next agent, and when to send the item onward, back, or to a human. Start with "Your step is **<name>**.".
"#,
        models = models.join(", "),
        efforts = EFFORT_LEVELS.join(", "),
        max_workers = MAX_WORKERS,
        tools = KNOWN_TOOLS.join(", "),
        modes = PERMISSION_MODES.join(", "),
    )
}

/// Parses the model's answer (tolerating code fences and surrounding text)
/// and checks it is a valid pipeline that only uses offered models and
/// known tools and permission modes.
pub fn parse(text: &str, models: &[String]) -> Result<Generated> {
    let start = text.find('{').context("the answer contains no JSON object")?;
    let end = text.rfind('}').context("the answer contains no JSON object")?;
    if end < start {
        bail!("the answer contains no JSON object");
    }
    let g: Generated = serde_json::from_str(&text[start..=end]).context("the answer is not a valid pipeline JSON object")?;
    if g.max_iterations > 100 {
        bail!("max_iterations must be at most 100");
    }
    for gs in &g.steps {
        let s = &gs.step;
        if !models.is_empty() && !models.iter().any(|m| m == &s.model) {
            bail!("step '{}' uses model '{}': use one of {}", s.id, s.model, models.join(", "));
        }
        if !PERMISSION_MODES.contains(&s.permission_mode.as_str()) {
            bail!("step '{}' has unknown permission mode '{}'", s.id, s.permission_mode);
        }
        if s.max_workers > MAX_WORKERS {
            bail!("step '{}' has more than {MAX_WORKERS} workers", s.id);
        }
        if let Some(t) = s.tools.iter().find(|t| !KNOWN_TOOLS.contains(&t.as_str())) {
            bail!("step '{}' has unknown tool '{}'", s.id, t);
        }
    }
    let pipeline = Pipeline {
        max_iterations: g.max_iterations,
        models: models.to_vec(),
        steps: g.steps.iter().map(|s| s.step.clone()).collect(),
    };
    pipeline.validate()?;
    Ok(g)
}

/// Asks `claude` (with `model`) for a pipeline matching `request`. Retries
/// once with the error when the first answer is invalid.
pub async fn generate(claude: &str, cwd: &Path, model: &str, request: &str, models: &[String]) -> Result<Generated> {
    let request = request.trim();
    if request.is_empty() {
        bail!("describe the pipeline you want");
    }
    if model.trim().is_empty() {
        bail!("choose a model");
    }
    let system = system_prompt(models);
    let first = ask(claude, cwd, model, &system, request).await?;
    let err = match parse(&first, models) {
        Ok(g) => return Ok(g),
        Err(e) => e,
    };
    tracing::info!("generated pipeline was invalid ({err:#}), asking again");
    let retry = format!(
        "{request}\n\n---\nYour previous answer was rejected: {err:#}\n\nPrevious answer:\n{first}\n\n\
         Answer again with only the corrected JSON object."
    );
    let second = ask(claude, cwd, model, &system, &retry).await?;
    parse(&second, models).context("the model did not produce a valid pipeline (try again or rephrase)")
}

/// One `claude -p` call without tools; returns the result text.
async fn ask(claude: &str, cwd: &Path, model: &str, system: &str, prompt: &str) -> Result<String> {
    let mut cmd = Command::new(claude);
    cmd.args([
        "-p",
        "--output-format",
        "json",
        "--strict-mcp-config",
        "--tools",
        "",
        "--permission-mode",
        "dontAsk",
        "--model",
        model,
        "--append-system-prompt",
        system,
    ])
    .current_dir(cwd)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    crate::runner::scrub_env(&mut cmd);
    let mut child = cmd.spawn().with_context(|| format!("starting {claude}"))?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    stdin.write_all(prompt.as_bytes()).await?;
    drop(stdin);
    let out = tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("generating the pipeline took longer than {} seconds", TIMEOUT.as_secs()))??;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: Value = match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(_) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("claude failed ({}): {}", out.status, stderr.trim().lines().last().unwrap_or(stdout.trim()));
        }
    };
    let result = v.get("result").and_then(Value::as_str).unwrap_or_default().to_string();
    if v.get("is_error").and_then(Value::as_bool).unwrap_or(false) || !out.status.success() {
        bail!("claude failed: {}", if result.is_empty() { out.status.to_string() } else { result });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models() -> Vec<String> {
        vec!["claude-opus-5-5".into(), "claude-haiku-5-5".into()]
    }

    const GOOD: &str = r#"{
      "max_iterations": 4,
      "steps": [
        {"id": "design", "name": "Design", "model": "claude-opus-5-5", "max_workers": 2,
         "allowed_targets": ["next", "human"], "tools": ["Read", "Write", "Edit"],
         "permission_mode": "dontAsk", "prompt": "Your step is **Design**."},
        {"id": "check", "name": "Check", "model": "claude-haiku-5-5", "effort": "low",
         "allowed_targets": ["next", "design", "human"], "tools": ["Read", "Bash", "Write"]}
      ]
    }"#;

    #[test]
    fn parses_valid_answer_with_defaults() {
        let g = parse(GOOD, &models()).unwrap();
        assert_eq!(g.max_iterations, 4);
        assert_eq!(g.steps.len(), 2);
        assert_eq!(g.steps[0].prompt, "Your step is **Design**.");
        assert_eq!(g.steps[1].step.max_workers, 1);
        assert_eq!(g.steps[1].step.permission_mode, "dontAsk");
        assert_eq!(g.steps[1].step.effort.as_deref(), Some("low"));
        assert_eq!(g.steps[1].prompt, "");
        // Serializes back flat, the way the UI expects a step.
        let v = serde_json::to_value(&g).unwrap();
        assert_eq!(v["steps"][0]["id"], "design");
        assert_eq!(v["steps"][0]["prompt"], "Your step is **Design**.");
    }

    #[test]
    fn strips_code_fences_and_prose() {
        let text = format!("Here is your pipeline:\n```json\n{GOOD}\n```\nEnjoy!");
        assert_eq!(parse(&text, &models()).unwrap().steps.len(), 2);
    }

    #[test]
    fn rejects_invalid_answers() {
        let m = models();
        assert!(parse("no json here", &m).is_err());
        assert!(parse("{ not json }", &m).is_err());
        assert!(parse(r#"{"steps": []}"#, &m).is_err());
        let bad = |from: &str, to: &str| parse(&GOOD.replacen(from, to, 1), &m);
        assert!(bad("claude-haiku-5-5", "gpt-4").is_err(), "unknown model");
        assert!(bad(r#""Read", "Write", "Edit""#, r#""Read", "Edit""#).is_err(), "missing Write");
        assert!(bad(r#""tools": ["Read", "Write", "Edit"]"#, r#""tools": ["Write", "Teleport"]"#).is_err(), "unknown tool");
        assert!(bad(r#""permission_mode": "dontAsk""#, r#""permission_mode": "yolo""#).is_err());
        assert!(bad(r#""max_workers": 2"#, r#""max_workers": 99"#).is_err());
        assert!(bad(r#""next", "design", "human""#, r#""next", "nowhere""#).is_err(), "unknown target");
        assert!(bad(r#""id": "check""#, r#""id": "done""#).is_err(), "reserved id");
        assert!(bad(r#""effort": "low""#, r#""effort": "turbo""#).is_err());
        assert!(bad(r#""max_iterations": 4"#, r#""max_iterations": 1000"#).is_err());
    }
}

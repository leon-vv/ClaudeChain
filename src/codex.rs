//! Runs agents on OpenAI models through the Codex CLI (`codex exec --json`).
//!
//! Codex prints its own JSONL events. `Translator` turns them into the Claude
//! stream-json shapes the rest of ClaudeChain already understands (session
//! tracking, the token meter, the web UI's transcript). The raw Codex events
//! are kept in events.jsonl next to the translated ones.
//!
//! Differences from Claude: Codex reads one prompt from stdin and takes no
//! further input, so human messages sent during a run are kept for the next
//! run. It reports no cost: the API-equivalent cost is estimated from the
//! OpenAI API prices in `price`. Its plan usage is
//! read from its session logs (`read_limits`). It has no per-tool allow
//! list; the permission mode picks its sandbox instead.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use serde_json::{Value, json};

/// OpenAI API prices in USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input: f64,
    pub cached_input: f64,
    /// Cache writes. Codex doesn't say which cache duration a write used, so
    /// this is the 30-minute write price (the input price where none is listed).
    pub cache_write: f64,
    pub output: f64,
}

impl Price {
    /// Cost of a run's tokens; `input` excludes cached input and cache writes.
    pub fn cost(&self, input: u64, cached: u64, cache_write: u64, output: u64) -> f64 {
        (input as f64 * self.input
            + cached as f64 * self.cached_input
            + cache_write as f64 * self.cache_write
            + output as f64 * self.output)
            / 1_000_000.0
    }
}

/// API prices of the OpenAI models, for the API-equivalent cost of Codex
/// runs. Where no cached-input or cache-write price exists the input price is
/// used. None for unknown models (their runs show no cost).
pub fn price(model: &str) -> Option<Price> {
    // (model, input, output, cached input, 30-minute cache write)
    const PRICES: &[(&str, f64, f64, Option<f64>, Option<f64>)] = &[
        ("gpt-6.2-astra", 8.00, 40.00, Some(0.40), Some(10.00)),
        ("gpt-6.2-sol", 2.00, 10.00, Some(0.10), Some(2.50)),
        ("gpt-6-astra", 10.00, 50.00, Some(1.00), Some(12.50)),
        ("gpt-6.1-sol", 2.00, 10.00, Some(0.10), Some(2.50)),
        ("gpt-6.1-terra", 1.00, 5.00, Some(0.05), Some(1.25)),
        ("gpt-6-luna", 0.10, 0.50, Some(0.01), Some(0.125)),
        ("gpt-6-sol", 2.00, 10.00, Some(0.20), Some(2.50)),
        ("gpt-5.6-sol", 4.00, 20.00, Some(0.40), Some(5.00)),
        ("gpt-5.6-terra", 2.00, 12.00, Some(0.20), Some(2.50)),
        ("gpt-5.6-luna", 0.20, 1.20, Some(0.02), Some(0.25)),
        ("gpt-5.5", 5.00, 30.00, Some(0.50), None),
        ("gpt-5.5-pro", 30.00, 180.00, None, None),
        ("gpt-5.4", 2.50, 15.00, Some(0.25), None),
        ("gpt-5.4-pro", 30.00, 180.00, None, None),
    ];
    let model = model.trim().to_ascii_lowercase();
    PRICES.iter().find(|p| p.0 == model).map(|&(_, input, output, cached, write)| Price {
        input,
        cached_input: cached.unwrap_or(input),
        cache_write: write.unwrap_or(input),
        output,
    })
}

/// What `args` needs to build a `codex exec` command line.
pub struct Invocation<'a> {
    pub model: &'a str,
    pub effort: Option<&'a str>,
    /// The step's Claude permission mode, mapped to a Codex sandbox.
    pub permission_mode: &'a str,
    pub system_prompt: &'a str,
    /// Directories outside the working directory the agent may write to.
    /// Only existing directories are passed (Codex fails on missing ones).
    pub writable_roots: &'a [PathBuf],
    /// Thread id of an earlier run to continue.
    pub resume: Option<&'a str>,
}

/// The Codex sandbox for a Claude permission mode.
pub fn sandbox(permission_mode: &str) -> &'static str {
    match permission_mode {
        "plan" => "read-only",
        "bypassPermissions" => "danger-full-access",
        _ => "workspace-write",
    }
}

/// Arguments for `codex`. The prompt is read from stdin ("-").
/// Everything is passed as `-c` overrides because `codex exec resume` has no
/// `--sandbox` or `--add-dir` flags.
pub fn args(inv: &Invocation) -> Vec<String> {
    let mut a: Vec<String> = vec!["exec".into()];
    if inv.resume.is_some() {
        a.push("resume".into());
    }
    // The user's own config.toml may enable plugins and MCP servers; agents
    // get only the built-in tools (like --strict-mcp-config for Claude). The
    // login lives in auth.json, so it still applies.
    a.extend(["--json", "--skip-git-repo-check", "--ignore-user-config", "-m", inv.model].map(String::from));
    let mut set = |key: &str, value: String| {
        a.push("-c".into());
        a.push(format!("{key}={value}"));
    };
    let sandbox = sandbox(inv.permission_mode);
    set("sandbox_mode", toml_str(sandbox));
    set("approval_policy", toml_str("never"));
    if sandbox == "workspace-write" {
        // Agents fetch dependencies, use git remotes and gh.
        set("sandbox_workspace_write.network_access", "true".into());
        let roots: Vec<String> = inv
            .writable_roots
            .iter()
            .filter(|p| p.is_dir())
            .map(|p| toml_str(&p.display().to_string()))
            .collect();
        if !roots.is_empty() {
            set("sandbox_workspace_write.writable_roots", format!("[{}]", roots.join(",")));
        }
    }
    if let Some(effort) = inv.effort {
        set("model_reasoning_effort", toml_str(effort));
    }
    set("developer_instructions", toml_str(inv.system_prompt));
    if let Some(id) = inv.resume {
        a.push(id.into());
    }
    a.push("-".into());
    a
}

/// A TOML basic string. JSON string escapes are valid TOML escapes.
fn toml_str(s: &str) -> String {
    serde_json::to_string(s).expect("string serializes")
}

/// Turns Codex `exec --json` events into Claude stream-json events.
pub struct Translator {
    model: String,
    sandbox: &'static str,
    started: Instant,
    /// Tool items whose tool_use event was already sent.
    open_tools: HashSet<String>,
    last_message: Option<String>,
    session: Option<String>,
    turns: u32,
    price: Option<Price>,
}

impl Translator {
    pub fn new(model: &str, permission_mode: &str, price: Option<Price>) -> Self {
        Self {
            model: model.into(),
            sandbox: sandbox(permission_mode),
            started: Instant::now(),
            open_tools: HashSet::new(),
            last_message: None,
            session: None,
            turns: 0,
            price,
        }
    }

    /// The Claude-shaped events for one Codex event (often none).
    pub fn translate(&mut self, ev: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        match ev["type"].as_str().unwrap_or("") {
            "thread.started" => {
                let id = ev["thread_id"].as_str().unwrap_or_default().to_string();
                out.push(tag(json!({
                    "type": "system",
                    "subtype": "init",
                    "session_id": id,
                    "model": self.model,
                    "permissionMode": format!("codex sandbox {}", self.sandbox),
                    "tools": [],
                })));
                self.session = Some(id);
            }
            "turn.started" => self.turns += 1,
            "item.started" => {
                let item = &ev["item"];
                if let Some((name, input)) = tool_call(item) {
                    self.open_tool(item, name, input, &mut out);
                }
            }
            "item.completed" => self.item_completed(&ev["item"], &mut out),
            "turn.completed" => out.push(self.result(false, &ev["usage"], None)),
            "turn.failed" => {
                let msg = ev["error"]["message"].as_str().unwrap_or("turn failed").to_string();
                out.push(self.result(true, &Value::Null, Some(msg)));
            }
            "error" => {
                let msg = ev["message"].as_str().unwrap_or("error");
                out.push(warning(&format!("Codex: {msg}")));
            }
            _ => {}
        }
        out
    }

    fn item_completed(&mut self, item: &Value, out: &mut Vec<Value>) {
        let id = item["id"].as_str().unwrap_or_default().to_string();
        match item["type"].as_str().unwrap_or("") {
            "agent_message" => {
                let text = item["text"].as_str().unwrap_or_default().to_string();
                out.push(assistant(json!({"type": "text", "text": text})));
                self.last_message = Some(text);
            }
            "reasoning" => {
                let text = item["text"].as_str().unwrap_or_default();
                if !text.trim().is_empty() {
                    out.push(assistant(json!({"type": "thinking", "thinking": text})));
                }
            }
            "error" => {
                let msg = item["message"].as_str().unwrap_or("error");
                out.push(warning(&format!("Codex: {msg}")));
            }
            _ => {
                let Some((name, input)) = tool_call(item) else { return };
                if !self.open_tools.contains(&id) {
                    self.open_tool(item, name, input, out);
                }
                self.open_tools.remove(&id);
                let (content, is_error) = tool_output(item);
                out.push(tag(json!({
                    "type": "user",
                    "message": {"role": "user", "content": [{
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": content,
                        "is_error": is_error,
                    }]},
                })));
            }
        }
    }

    fn open_tool(&mut self, item: &Value, name: String, input: Value, out: &mut Vec<Value>) {
        let id = item["id"].as_str().unwrap_or_default().to_string();
        out.push(assistant(json!({"type": "tool_use", "id": id, "name": name, "input": input})));
        self.open_tools.insert(id);
    }

    fn result(&self, is_error: bool, usage: &Value, error: Option<String>) -> Value {
        // OpenAI counts cached tokens (and cache writes) inside input_tokens,
        // and reasoning tokens inside output_tokens; Claude counts them apart.
        let n = |k: &str| usage[k].as_u64().unwrap_or(0);
        let cached = n("cached_input_tokens");
        let cache_write = n("cache_write_input_tokens");
        let input = n("input_tokens").saturating_sub(cached + cache_write);
        let output = n("output_tokens");
        let cost = self.price.map(|p| p.cost(input, cached, cache_write, output));
        tag(json!({
            "type": "result",
            "subtype": if is_error { "error" } else { "success" },
            "is_error": is_error,
            "result": error.or_else(|| self.last_message.clone()).unwrap_or_default(),
            "session_id": self.session,
            "num_turns": self.turns.max(1),
            "duration_ms": self.started.elapsed().as_millis() as u64,
            "total_cost_usd": cost,
            "usage": {
                "input_tokens": input,
                "cache_read_input_tokens": cached,
                "cache_creation_input_tokens": cache_write,
                "output_tokens": output,
            },
        }))
    }
}

/// Tool name and input for items that are tool calls.
fn tool_call(item: &Value) -> Option<(String, Value)> {
    Some(match item["type"].as_str()? {
        "command_execution" => ("Bash".into(), json!({"command": item["command"]})),
        "file_change" => {
            let changes = item["changes"].as_array().cloned().unwrap_or_default();
            let paths: Vec<&str> = changes.iter().filter_map(|c| c["path"].as_str()).collect();
            ("Edit".into(), json!({"file_path": paths.join(", "), "changes": changes}))
        }
        "mcp_tool_call" => (
            format!("{}.{}", item["server"].as_str().unwrap_or("mcp"), item["tool"].as_str().unwrap_or("tool")),
            item["arguments"].clone(),
        ),
        "web_search" => ("WebSearch".into(), json!({"query": item["query"]})),
        "todo_list" => ("TodoWrite".into(), json!({"items": item["items"]})),
        _ => return None,
    })
}

/// Result text of a finished tool item and whether it failed.
fn tool_output(item: &Value) -> (String, bool) {
    let failed = item["status"] == "failed";
    match item["type"].as_str().unwrap_or("") {
        "command_execution" => {
            let code = item["exit_code"].as_i64();
            let mut text = item["aggregated_output"].as_str().unwrap_or_default().to_string();
            if let Some(c) = code.filter(|c| *c != 0) {
                text.push_str(&format!("\n(exit code {c})"));
            }
            (text, failed || code.is_some_and(|c| c != 0))
        }
        "mcp_tool_call" => {
            if let Some(e) = item["error"]["message"].as_str() {
                (e.to_string(), true)
            } else {
                (serde_json::to_string_pretty(&item["result"]).unwrap_or_default(), failed)
            }
        }
        _ => (item["status"].as_str().unwrap_or("done").to_string(), failed),
    }
}

fn assistant(block: Value) -> Value {
    tag(json!({"type": "assistant", "message": {"role": "assistant", "content": [block]}}))
}

fn warning(text: &str) -> Value {
    let mut ev = crate::runner::local_event("codex_error", text);
    ev["source"] = "codex".into();
    ev
}

/// Marks translated events so the UI can tell them apart.
fn tag(mut ev: Value) -> Value {
    ev["source"] = "codex".into();
    ev
}

/// Codex's home: `$CODEX_HOME` or `~/.codex`.
fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
}

/// The newest session log (`sessions/YYYY/MM/DD/rollout-*.jsonl`) and its
/// modification time. A session that started yesterday may still be the one
/// written to, so the two newest day folders are searched.
pub fn newest_rollout() -> Option<(PathBuf, std::time::SystemTime)> {
    let sorted_dirs = |p: &std::path::Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        v.sort();
        v
    };
    let mut days = Vec::new();
    for year in sorted_dirs(&codex_home()?.join("sessions")).iter().rev() {
        for month in sorted_dirs(year).iter().rev() {
            for day in sorted_dirs(month).iter().rev() {
                days.push(day.clone());
                if days.len() == 2 {
                    break;
                }
            }
            if days.len() == 2 {
                break;
            }
        }
        if days.len() == 2 {
            break;
        }
    }
    days.iter()
        .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten())
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .max_by_key(|(_, t)| *t)
}

/// The latest plan usage in a Codex session log, as (time, info) where info
/// has the same `unifiedWindows` shape Claude Code reports, so the UI and the
/// pause logic can treat both alike. Only the end of the file is read.
pub fn read_limits(path: &std::path::Path) -> Option<(chrono::DateTime<chrono::Utc>, Value)> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 512 * 1024;
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    text.lines().rev().filter(|l| l.contains("\"rate_limits\"")).find_map(|l| {
        let ev: Value = serde_json::from_str(l).ok()?;
        let info = limits_info(&ev["payload"]["rate_limits"])?;
        let at = ev["timestamp"]
            .as_str()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);
        Some((at, info))
    })
}

/// Codex `rate_limits` (primary/secondary windows with used_percent,
/// window_minutes, resets_at) as Claude-style `unifiedWindows`.
fn limits_info(rl: &Value) -> Option<Value> {
    let mut windows = serde_json::Map::new();
    for key in ["primary", "secondary"] {
        let w = &rl[key];
        let Some(used) = w["used_percent"].as_f64() else { continue };
        let name = match w["window_minutes"].as_u64() {
            Some(300) => "five_hour".to_string(),
            Some(10080) => "seven_day".to_string(),
            Some(m) => format!("{m}_minutes"),
            None => key.to_string(),
        };
        windows.insert(
            name,
            json!({
                "utilization": used / 100.0,
                "resetsAt": w["resets_at"],
                "status": if used >= 100.0 { "rejected" } else { "allowed" },
            }),
        );
    }
    if windows.is_empty() {
        return None;
    }
    Some(json!({"unifiedWindows": windows, "plan": rl["plan_type"], "source": "codex"}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(t: &mut Translator, lines: &str) -> Vec<Value> {
        lines.lines().flat_map(|l| t.translate(&serde_json::from_str(l).unwrap())).collect()
    }

    #[test]
    fn translates_a_run() {
        let mut t = Translator::new("gpt-6.2-sol", "dontAsk", Some(Price { input: 1.0, cached_input: 0.1, cache_write: 1.25, output: 10.0 }));
        let out = run(
            &mut t,
            r#"{"type":"thread.started","thread_id":"abc"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Hi"}}
{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"echo hello","aggregated_output":"","exit_code":null,"status":"in_progress"}}
{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"echo hello","aggregated_output":"hello\n","exit_code":0,"status":"completed"}}
{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"Done"}}
{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":60,"cache_write_input_tokens":0,"output_tokens":7,"reasoning_output_tokens":3}}"#,
        );
        assert_eq!(out[0]["type"], "system");
        assert_eq!(out[0]["session_id"], "abc");
        assert_eq!(out[1]["message"]["content"][0]["text"], "Hi");
        assert_eq!(out[2]["message"]["content"][0]["type"], "tool_use");
        assert_eq!(out[2]["message"]["content"][0]["input"]["command"], "echo hello");
        assert_eq!(out[3]["message"]["content"][0]["tool_use_id"], "item_1");
        assert_eq!(out[3]["message"]["content"][0]["is_error"], false);
        let result = out.last().unwrap();
        assert_eq!(result["type"], "result");
        assert_eq!(result["is_error"], false);
        assert_eq!(result["result"], "Done");
        assert_eq!(result["session_id"], "abc");
        assert_eq!(result["usage"]["input_tokens"], 40);
        assert_eq!(result["usage"]["cache_read_input_tokens"], 60);
        assert_eq!(result["usage"]["output_tokens"], 7);
        // 40 * 1.0 + 60 * 0.1 + 7 * 10.0 = 116 USD per million tokens
        assert!((result["total_cost_usd"].as_f64().unwrap() - 116e-6).abs() < 1e-12);
        assert_eq!(out.len(), 6);
    }

    #[test]
    fn failed_turn_is_an_error_result() {
        let mut t = Translator::new("nope", "dontAsk", None);
        let out = run(
            &mut t,
            r#"{"type":"error","message":"model not supported"}
{"type":"turn.failed","error":{"message":"model not supported"}}"#,
        );
        assert_eq!(out[0]["type"], "claudechain");
        assert_eq!(out[1]["type"], "result");
        assert_eq!(out[1]["is_error"], true);
        assert_eq!(out[1]["result"], "model not supported");
        assert!(out[1]["total_cost_usd"].is_null());
    }

    #[test]
    fn failed_file_change_without_start() {
        let mut t = Translator::new("m", "dontAsk", None);
        let out = run(
            &mut t,
            r#"{"type":"item.completed","item":{"id":"item_3","type":"file_change","changes":[{"path":"/x.txt","kind":"add"}],"status":"failed"}}"#,
        );
        assert_eq!(out[0]["message"]["content"][0]["name"], "Edit");
        assert_eq!(out[1]["message"]["content"][0]["is_error"], true);
    }

    #[test]
    fn known_models_have_prices() {
        let p = price("gpt-6.2-sol").unwrap();
        assert_eq!((p.input, p.cached_input, p.cache_write, p.output), (2.0, 0.1, 2.5, 10.0));
        let pro = price("gpt-5.5-pro").unwrap();
        assert_eq!((pro.cached_input, pro.cache_write), (30.0, 30.0));
        assert!(price("gpt-unknown").is_none());
        // 1M uncached input + 1M cached + 1M cache write + 1M output
        assert!((p.cost(1_000_000, 1_000_000, 1_000_000, 1_000_000) - 14.6).abs() < 1e-9);
    }

    #[test]
    fn reads_limits_from_a_session_log() {
        let dir = std::env::temp_dir().join(format!("claudechain-codex-limits-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl");
        std::fs::write(
            &path,
            r#"{"timestamp":"2026-11-19T15:00:00.000Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":10.0,"window_minutes":300,"resets_at":1}}}}
{"timestamp":"2026-11-19T15:40:00.000Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex","primary":{"used_percent":2.0,"window_minutes":300,"resets_at":1795147299},"secondary":{"used_percent":100.0,"window_minutes":10080,"resets_at":1795734099},"plan_type":"plus"}}}
{"timestamp":"2026-11-19T15:41:00.000Z","type":"event_msg","payload":{"type":"agent_message"}}
"#,
        )
        .unwrap();
        let (at, info) = read_limits(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(at.to_rfc3339(), "2026-11-19T15:40:00+00:00");
        let w = &info["unifiedWindows"];
        assert_eq!(w["five_hour"]["utilization"], 0.02);
        assert_eq!(w["five_hour"]["resetsAt"], 1795147299);
        assert_eq!(w["seven_day"]["status"], "rejected");
    }

    #[test]
    fn builds_args() {
        let roots = [std::env::temp_dir(), PathBuf::from("/does/not/exist")];
        let a = args(&Invocation {
            model: "gpt-6.2-sol",
            effort: Some("high"),
            permission_mode: "dontAsk",
            system_prompt: "Be \"nice\"\nalways",
            writable_roots: &roots,
            resume: Some("abc"),
        });
        assert_eq!(&a[..2], ["exec", "resume"]);
        assert!(a.contains(&"sandbox_mode=\"workspace-write\"".to_string()));
        assert!(a.contains(&"model_reasoning_effort=\"high\"".to_string()));
        assert!(a.contains(&"developer_instructions=\"Be \\\"nice\\\"\\nalways\"".to_string()));
        let roots_arg = a.iter().find(|x| x.starts_with("sandbox_workspace_write.writable_roots=")).unwrap();
        assert!(!roots_arg.contains("/does/not/exist"));
        assert_eq!(&a[a.len() - 2..], ["abc", "-"]);

        let plan = args(&Invocation {
            model: "m",
            effort: None,
            permission_mode: "plan",
            system_prompt: "",
            writable_roots: &[],
            resume: None,
        });
        assert_eq!(plan[1], "--json");
        assert!(plan.contains(&"sandbox_mode=\"read-only\"".to_string()));
        assert!(!plan.iter().any(|x| x.starts_with("sandbox_workspace_write")));
    }
}

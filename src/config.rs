use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::store::write_atomic;

/// The pipeline definition, stored as `pipeline.toml` in the data directory.
/// Prompts live next to it in `prompts/general.md` and `prompts/<step id>.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pipeline {
    /// Upper bound on backward moves per item (e.g. build failed → implementation).
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// Models offered in the UI dropdowns.
    #[serde(default = "default_models")]
    pub models: Vec<String>,
    #[serde(rename = "step", alias = "steps")]
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub name: String,
    pub model: String,
    /// Thinking level passed to `claude --effort` (low, medium, high, xhigh, max).
    /// None uses Claude's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default = "default_workers")]
    pub max_workers: usize,
    #[serde(default)]
    pub require_approval: bool,
    /// Where the agent may send the item: "next", "human", or step ids.
    pub allowed_targets: Vec<String>,
    /// Built-in Claude Code tools available to (and pre-approved for) the agent.
    pub tools: Vec<String>,
    /// Passed to `claude --permission-mode`.
    #[serde(default = "default_permission_mode")]
    pub permission_mode: String,
}

fn default_max_iterations() -> u32 {
    6
}
fn default_workers() -> usize {
    1
}
fn default_permission_mode() -> String {
    "dontAsk".into()
}
fn default_models() -> Vec<String> {
    ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-1"]
        .map(String::from)
        .to_vec()
}

/// Accepted values for `Step::effort` (`claude --effort`).
pub const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

const DEFAULT_PIPELINE: &str = include_str!("../defaults/pipeline.toml");
const DEFAULT_PROMPTS: &[(&str, &str)] = &[
    ("general", include_str!("../defaults/prompts/general.md")),
    ("triage", include_str!("../defaults/prompts/triage.md")),
    ("breakdown", include_str!("../defaults/prompts/breakdown.md")),
    ("implementation", include_str!("../defaults/prompts/implementation.md")),
    ("build_test", include_str!("../defaults/prompts/build_test.md")),
    ("review", include_str!("../defaults/prompts/review.md")),
    ("pull_request", include_str!("../defaults/prompts/pull_request.md")),
];

impl Pipeline {
    pub fn step(&self, id: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.id == id)
    }

    pub fn step_mut(&mut self, id: &str) -> Option<&mut Step> {
        self.steps.iter_mut().find(|s| s.id == id)
    }

    pub fn step_index(&self, id: &str) -> Option<usize> {
        self.steps.iter().position(|s| s.id == id)
    }

    /// The step after `id`, or None when `id` is the last step.
    pub fn next_step(&self, id: &str) -> Option<&Step> {
        self.step_index(id).and_then(|i| self.steps.get(i + 1))
    }

    pub fn validate(&self) -> Result<()> {
        if self.steps.is_empty() {
            bail!("pipeline has no steps");
        }
        for (i, s) in self.steps.iter().enumerate() {
            if self.steps[..i].iter().any(|o| o.id == s.id) {
                bail!("duplicate step id '{}'", s.id);
            }
            let id_ok = !s.id.is_empty()
                && s.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
            if !id_ok || matches!(s.id.as_str(), "general" | "next" | "human" | "done") {
                bail!("invalid step id '{}': use lowercase letters, digits, '_' or '-' (reserved: general, next, human, done)", s.id);
            }
            if s.name.trim().is_empty() {
                bail!("step '{}' needs a name", s.id);
            }
            if s.model.trim().is_empty() {
                bail!("step '{}' needs a model", s.id);
            }
            if let Some(e) = &s.effort
                && !EFFORT_LEVELS.contains(&e.as_str())
            {
                bail!("step '{}' has unknown effort '{}': use one of {}", s.id, e, EFFORT_LEVELS.join(", "));
            }
            if !s.tools.iter().any(|t| t == "Write") {
                bail!("step '{}' needs the Write tool to write its message and transition files", s.name);
            }
            if s.allowed_targets.is_empty() {
                bail!("step '{}' has no allowed transitions", s.name);
            }
            for t in &s.allowed_targets {
                if t != "next" && t != "human" && !self.steps.iter().any(|o| &o.id == t) {
                    bail!("step '{}' has unknown target '{}'", s.id, t);
                }
            }
        }
        Ok(())
    }
}

pub fn pipeline_path(data: &Path) -> PathBuf {
    data.join("pipeline.toml")
}

pub fn prompt_path(data: &Path, id: &str) -> PathBuf {
    data.join("prompts").join(format!("{id}.md"))
}

/// Loads the pipeline, writing the defaults first when the data dir is new.
pub fn load_or_init(data: &Path) -> Result<Pipeline> {
    let path = pipeline_path(data);
    if !path.exists() {
        std::fs::create_dir_all(data.join("prompts"))?;
        write_atomic(&path, DEFAULT_PIPELINE.as_bytes())?;
        for (id, text) in DEFAULT_PROMPTS {
            let p = prompt_path(data, id);
            if !p.exists() {
                write_atomic(&p, text.as_bytes())?;
            }
        }
        tracing::info!("wrote default pipeline to {}", path.display());
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let pipeline: Pipeline = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    pipeline.validate()?;
    Ok(pipeline)
}

pub fn save(data: &Path, pipeline: &Pipeline) -> Result<()> {
    let text = toml::to_string_pretty(pipeline)?;
    write_atomic(&pipeline_path(data), text.as_bytes())
}

/// Starting prompt for a step added from the UI.
pub fn stub_prompt(name: &str) -> String {
    format!(
        "Your step is **{name}**.\n\nDescribe what this agent should do, what it must not do, \
         and when to send the item onward, back, or to a human.\n"
    )
}

pub fn read_prompt(data: &Path, id: &str) -> String {
    std::fs::read_to_string(prompt_path(data, id)).unwrap_or_default()
}

pub fn write_prompt(data: &Path, id: &str, text: &str) -> Result<()> {
    std::fs::create_dir_all(data.join("prompts"))?;
    write_atomic(&prompt_path(data, id), text.as_bytes())
}

/// Version of the shareable pipeline file written by [`export_bundle`].
pub const BUNDLE_FORMAT_VERSION: i64 = 1;

/// A pipeline together with all of its prompts, as read from a shared file.
#[derive(Debug, Clone)]
pub struct Bundle {
    pub pipeline: Pipeline,
    /// The general prompt, when the file has one.
    pub general_prompt: Option<String>,
    /// One prompt per step, in step order: (step id, text).
    pub prompts: Vec<(String, String)>,
}

/// Writes the pipeline and all of its prompts as one self-contained TOML file
/// that can be shared and imported with [`parse_bundle`]. Machine-local state
/// (workspace path, items, usage limits) is not included.
pub fn export_bundle(data: &Path, pipeline: &Pipeline) -> Result<String> {
    let toml::Value::Table(mut table) = toml::Value::try_from(pipeline).context("serializing pipeline")? else {
        bail!("pipeline did not serialize to a table");
    };
    table.insert("format_version".into(), toml::Value::Integer(BUNDLE_FORMAT_VERSION));
    table.insert("general_prompt".into(), toml::Value::String(read_prompt(data, "general")));
    if let Some(toml::Value::Array(steps)) = table.get_mut("step") {
        for step in steps.iter_mut() {
            if let toml::Value::Table(t) = step {
                let id = t.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                t.insert("prompt".into(), toml::Value::String(read_prompt(data, &id)));
            }
        }
    }
    let body = toml::to_string_pretty(&table).context("writing pipeline file")?;
    Ok(format!(
        "# ClaudeChain pipeline: steps, settings and prompts.\n\
         # Import it from the pipeline editor (Edit pipeline details… → Import…).\n\n{body}"
    ))
}

/// Parses and validates a file written by [`export_bundle`]. Steps without a
/// prompt get the starter prompt.
pub fn parse_bundle(text: &str) -> Result<Bundle> {
    let mut table: toml::Table = toml::from_str(text).context("parsing pipeline file")?;
    match table.remove("format_version") {
        None => bail!("not a ClaudeChain pipeline file: format_version is missing"),
        Some(toml::Value::Integer(v)) if v == BUNDLE_FORMAT_VERSION => {}
        Some(v) => bail!("unsupported pipeline file format_version {v} (this version reads {BUNDLE_FORMAT_VERSION})"),
    }
    let general_prompt = match table.remove("general_prompt") {
        None => None,
        Some(toml::Value::String(s)) => Some(s),
        Some(_) => bail!("general_prompt must be a string"),
    };
    let mut step_prompts = vec![];
    for key in ["step", "steps"] {
        if let Some(toml::Value::Array(steps)) = table.get_mut(key) {
            for step in steps.iter_mut() {
                let prompt = match step {
                    toml::Value::Table(t) => match t.remove("prompt") {
                        None => None,
                        Some(toml::Value::String(s)) => Some(s),
                        Some(_) => bail!("step prompt must be a string"),
                    },
                    _ => None,
                };
                step_prompts.push(prompt);
            }
        }
    }
    let pipeline: Pipeline = toml::Value::Table(table).try_into().context("reading pipeline from file")?;
    pipeline.validate()?;
    let prompts = pipeline
        .steps
        .iter()
        .zip(step_prompts.into_iter().chain(std::iter::repeat(None)))
        .map(|(s, p)| (s.id.clone(), p.unwrap_or_else(|| stub_prompt(&s.name))))
        .collect();
    Ok(Bundle { pipeline, general_prompt, prompts })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_pipeline_is_valid_and_has_four_steps() {
        let p: Pipeline = toml::from_str(DEFAULT_PIPELINE).unwrap();
        p.validate().unwrap();
        let steps: Vec<(&str, &str)> = p.steps.iter().map(|s| (s.id.as_str(), s.model.as_str())).collect();
        assert_eq!(
            steps,
            [
                ("triage", "claude-haiku-5-5"),
                ("implementation", "claude-opus-5-5"),
                ("build_test", "claude-haiku-5-5"),
                ("pull_request", "claude-haiku-5-5"),
            ]
        );
        for s in &p.steps {
            assert!(DEFAULT_PROMPTS.iter().any(|(id, _)| *id == s.id), "no default prompt for {}", s.id);
        }
    }

    #[test]
    fn effort_parses_and_validates() {
        let text = |effort: &str| {
            format!(
                "[[step]]\nid = \"a\"\nname = \"A\"\nmodel = \"m\"\n{effort}\
                 allowed_targets = [\"next\"]\ntools = [\"Write\"]\n"
            )
        };
        let p: Pipeline = toml::from_str(&text("")).unwrap();
        p.validate().unwrap();
        assert_eq!(p.steps[0].effort, None);
        assert!(!toml::to_string_pretty(&p).unwrap().contains("effort"));

        let p: Pipeline = toml::from_str(&text("effort = \"xhigh\"\n")).unwrap();
        p.validate().unwrap();
        assert_eq!(p.steps[0].effort.as_deref(), Some("xhigh"));

        let p: Pipeline = toml::from_str(&text("effort = \"turbo\"\n")).unwrap();
        assert!(p.validate().is_err());
    }

    fn temp_data() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "claudechain-bundle-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bundle_round_trips_default_pipeline_and_prompts() {
        let data = temp_data();
        let p = load_or_init(&data).unwrap();
        write_prompt(&data, "triage", "Triage prompt with \"\"\" quotes\nand 'lines'\n").unwrap();
        let text = export_bundle(&data, &p).unwrap();
        let b = parse_bundle(&text).unwrap();
        assert_eq!(toml::to_string(&b.pipeline).unwrap(), toml::to_string(&p).unwrap());
        assert_eq!(b.general_prompt.as_deref(), Some(read_prompt(&data, "general").as_str()));
        assert_eq!(b.prompts.len(), p.steps.len());
        for (id, prompt) in &b.prompts {
            assert_eq!(prompt, &read_prompt(&data, id), "prompt for {id}");
        }
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn bundle_missing_prompt_gets_stub_and_bad_files_are_rejected() {
        let ok = "format_version = 1\n[[step]]\nid = \"a\"\nname = \"A\"\nmodel = \"m\"\n\
                  allowed_targets = [\"next\"]\ntools = [\"Write\"]\n";
        let b = parse_bundle(ok).unwrap();
        assert_eq!(b.general_prompt, None);
        assert_eq!(b.prompts, vec![("a".to_string(), stub_prompt("A"))]);

        assert!(parse_bundle(&ok.replace("format_version = 1\n", "")).is_err());
        assert!(parse_bundle(&ok.replace("format_version = 1", "format_version = 2")).is_err());
        assert!(parse_bundle(&ok.replace("\"a\"", "\"../x\"")).is_err());
        assert!(parse_bundle("not toml [").is_err());
    }
}

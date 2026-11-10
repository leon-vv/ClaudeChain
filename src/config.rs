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
    #[serde(rename = "step")]
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub name: String,
    pub model: String,
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

    fn validate(&self) -> Result<()> {
        if self.steps.is_empty() {
            bail!("pipeline has no steps");
        }
        for (i, s) in self.steps.iter().enumerate() {
            if self.steps[..i].iter().any(|o| o.id == s.id) {
                bail!("duplicate step id '{}'", s.id);
            }
            if s.id == "general" || s.id.contains(['/', '\\', '.']) {
                bail!("invalid step id '{}'", s.id);
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

pub fn read_prompt(data: &Path, id: &str) -> String {
    std::fs::read_to_string(prompt_path(data, id)).unwrap_or_default()
}

pub fn write_prompt(data: &Path, id: &str, text: &str) -> Result<()> {
    std::fs::create_dir_all(data.join("prompts"))?;
    write_atomic(&prompt_path(data, id), text.as_bytes())
}

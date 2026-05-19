use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Legacy single-timeout default. Preserved so that callers who set
/// `BREHON_PLAN_EXTRACT_TIMEOUT_SECS` continue to get exactly the
/// behavior they had before the idle/max split: the legacy var is
/// honored as *both* the idle and max timeout, which reduces to the
/// original single-wall-clock semantics.
pub(crate) const DEFAULT_EXTRACTOR_TIMEOUT_SECS: u64 = 180;

/// Default "no output for this long" timeout. Catches hung extractors
/// (credential prompts, stuck API calls, deadlocked streams) fast
/// without penalizing legitimately slow but progressing extractions
/// on large plans — as long as the extractor keeps emitting *any*
/// output (even reasoning tokens), this timer resets.
///
/// Override: `BREHON_PLAN_EXTRACT_IDLE_TIMEOUT_SECS`.
pub(crate) const DEFAULT_EXTRACTOR_IDLE_TIMEOUT_SECS: u64 = 120;

/// Default absolute wall-clock ceiling. Exists purely as a runaway
/// backstop — even a legitimately progressing extraction should
/// complete well under 30 minutes; anything longer is almost
/// certainly pathological.
///
/// Override: `BREHON_PLAN_EXTRACT_MAX_TIMEOUT_SECS`.
pub(crate) const DEFAULT_EXTRACTOR_MAX_TIMEOUT_SECS: u64 = 30 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanTask {
    pub source_id: String,
    pub title: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub size: String,
    #[serde(default)]
    pub gate: String,
    #[serde(default)]
    pub source_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details_doc: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_reading: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanEpic {
    pub source_id: String,
    pub title: String,
    #[serde(default)]
    pub tasks: Vec<PlanTask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanPhase {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub epics: Vec<PlanEpic>,
    #[serde(default)]
    pub gate_task: Option<PlanTask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanDocument {
    pub title: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub stack: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub path: PathBuf,
    #[serde(default)]
    pub phases: Vec<PlanPhase>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExtractorKind {
    Claude,
    Codex,
    Gemini,
    Opencode,
}

#[derive(Debug, Clone)]
pub(crate) struct ExtractorLaunch {
    pub agent_name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhaseExtractionSection {
    pub id: String,
    pub title: String,
    pub heading: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChunkablePlanDocument {
    pub title: String,
    pub project: Option<String>,
    pub stack: Option<String>,
    pub target: Option<String>,
    pub path: PathBuf,
    pub status_context: Option<String>,
    pub phases: Vec<PhaseExtractionSection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskExtractionSection {
    pub source_id: String,
    pub title: String,
    pub heading: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExtractedTaskSection {
    pub source_id: String,
    pub title: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub size: String,
    #[serde(default)]
    pub gate: String,
    #[serde(default)]
    pub source_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details_doc: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_reading: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_refs: Vec<String>,
    #[serde(default)]
    pub phase_gate: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ImportedTaskRecord {
    pub brehon_task_id: String,
    pub phase_id: String,
    pub phase_title: String,
    pub source_epic_id: Option<String>,
    pub source_epic_title: Option<String>,
    pub task: PlanTask,
    pub is_phase_gate: bool,
}

pub(crate) struct ScopedEnv {
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl ScopedEnv {
    pub fn set(vars: &[(&'static str, String)]) -> Self {
        let mut saved = Vec::with_capacity(vars.len());
        for (key, value) in vars {
            saved.push((*key, std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        Self { saved }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for (key, value) in self.saved.iter().rev() {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}

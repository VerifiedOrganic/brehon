use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::parsing::*;
use super::types::*;
use super::ExtractMode;

/// Parse a positive-seconds env var, falling back to `default` when the
/// variable is unset, unparseable, or zero.
fn read_positive_secs_env(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

/// Legacy single-timeout resolution. Kept for callers that still think
/// in "one wall-clock"; most new code should use
/// [`extractor_idle_timeout`] + [`extractor_max_timeout`] instead.
#[allow(dead_code)] // kept for backward-compat with external callers; see module doc
pub(crate) fn extractor_timeout() -> Duration {
    // Preserve old behavior: if the legacy env var is set, that's the wall-clock.
    Duration::from_secs(read_positive_secs_env(
        "BREHON_PLAN_EXTRACT_TIMEOUT_SECS",
        DEFAULT_EXTRACTOR_TIMEOUT_SECS,
    ))
}

/// How long an extractor may go without producing *any* output before
/// we treat it as hung and kill it.
///
/// Preference order:
/// 1. `BREHON_PLAN_EXTRACT_IDLE_TIMEOUT_SECS` (new explicit control)
/// 2. `BREHON_PLAN_EXTRACT_TIMEOUT_SECS` (legacy: caller set a single
///    wall-clock budget; honor it here so behavior matches the old
///    single-timeout contract for users who haven't migrated)
/// 3. [`DEFAULT_EXTRACTOR_IDLE_TIMEOUT_SECS`]
pub(crate) fn extractor_idle_timeout() -> Duration {
    let legacy = std::env::var("BREHON_PLAN_EXTRACT_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0);
    let secs = read_positive_secs_env(
        "BREHON_PLAN_EXTRACT_IDLE_TIMEOUT_SECS",
        legacy.unwrap_or(DEFAULT_EXTRACTOR_IDLE_TIMEOUT_SECS),
    );
    Duration::from_secs(secs)
}

/// Absolute wall-clock ceiling for an extractor, regardless of whether
/// it's still producing output. Exists as a runaway backstop — the
/// primary hang detector is [`extractor_idle_timeout`].
///
/// Preference order:
/// 1. `BREHON_PLAN_EXTRACT_MAX_TIMEOUT_SECS` (new explicit control)
/// 2. `BREHON_PLAN_EXTRACT_TIMEOUT_SECS` (legacy)
/// 3. [`DEFAULT_EXTRACTOR_MAX_TIMEOUT_SECS`]
pub(crate) fn extractor_max_timeout() -> Duration {
    let legacy = std::env::var("BREHON_PLAN_EXTRACT_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0);
    let secs = read_positive_secs_env(
        "BREHON_PLAN_EXTRACT_MAX_TIMEOUT_SECS",
        legacy.unwrap_or(DEFAULT_EXTRACTOR_MAX_TIMEOUT_SECS),
    );
    Duration::from_secs(secs)
}

/// Resolved bounds for a single extractor run.
///
/// Constructed via [`extractor_bounds`], which clamps `idle` to never
/// exceed `max` — an idle timeout longer than the max ceiling is
/// meaningless.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExtractorBounds {
    pub idle: Duration,
    pub max: Duration,
}

pub(crate) fn extractor_bounds() -> ExtractorBounds {
    let max = extractor_max_timeout();
    let idle = extractor_idle_timeout().min(max);
    ExtractorBounds { idle, max }
}

pub(crate) fn extraction_schema() -> Value {
    serde_json::from_str(
        r#"{
          "type": "object",
          "additionalProperties": false,
          "required": ["title", "phases"],
          "properties": {
            "title": {"type": "string"},
            "project": {"type": ["string", "null"]},
            "stack": {"type": ["string", "null"]},
            "target": {"type": ["string", "null"]},
            "phases": {
              "type": "array",
              "minItems": 1,
              "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["id", "title", "epics"],
                "properties": {
                  "id": {"type": "string"},
                  "title": {"type": "string"},
                  "notes": {"type": "array", "items": {"type": "string"}},
                  "epics": {
                    "type": "array",
                    "items": {
                      "type": "object",
                      "additionalProperties": false,
                      "required": ["source_id", "title", "tasks"],
                      "properties": {
                        "source_id": {"type": "string"},
                        "title": {"type": "string"},
                        "tasks": {
                          "type": "array",
                          "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["source_id", "title", "dependencies", "size", "gate", "source_status"],
                            "properties": {
                              "source_id": {"type": "string"},
                              "title": {"type": "string"},
                              "dependencies": {"type": "array", "items": {"type": "string"}},
                              "size": {"type": "string"},
                              "gate": {"type": "string"},
                              "source_status": {"type": "string"},
                              "details_doc": {"type": ["string", "null"]},
                              "required_reading": {"type": "array", "items": {"type": "string"}},
                              "context_refs": {"type": "array", "items": {"type": "string"}}
                            }
                          }
                        }
                      }
                    }
                  },
                  "gate_task": {
                    "type": ["object", "null"],
                    "additionalProperties": false,
                    "required": ["source_id", "title", "dependencies", "size", "gate", "source_status"],
                    "properties": {
                      "source_id": {"type": "string"},
                      "title": {"type": "string"},
                      "dependencies": {"type": "array", "items": {"type": "string"}},
                      "size": {"type": "string"},
                      "gate": {"type": "string"},
                      "source_status": {"type": "string"},
                      "details_doc": {"type": ["string", "null"]},
                      "required_reading": {"type": "array", "items": {"type": "string"}},
                      "context_refs": {"type": "array", "items": {"type": "string"}}
                    }
                  }
                }
              }
            }
          }
        }"#,
    )
    .expect("extraction schema should be valid JSON")
}

pub(crate) fn phase_extraction_schema() -> Value {
    serde_json::from_str(
        r#"{
          "type": "object",
          "additionalProperties": false,
          "required": ["id", "title", "epics"],
          "properties": {
            "id": {"type": "string"},
            "title": {"type": "string"},
            "notes": {"type": "array", "items": {"type": "string"}},
            "epics": {
              "type": "array",
              "minItems": 1,
              "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["source_id", "title", "tasks"],
                "properties": {
                  "source_id": {"type": "string"},
                  "title": {"type": "string"},
                  "tasks": {
                    "type": "array",
                    "items": {
                      "type": "object",
                      "additionalProperties": false,
                      "required": ["source_id", "title", "dependencies", "size", "gate", "source_status"],
                      "properties": {
                        "source_id": {"type": "string"},
                        "title": {"type": "string"},
                        "dependencies": {"type": "array", "items": {"type": "string"}},
                        "size": {"type": "string"},
                        "gate": {"type": "string"},
                        "source_status": {"type": "string"},
                        "details_doc": {"type": ["string", "null"]},
                        "required_reading": {"type": "array", "items": {"type": "string"}},
                        "context_refs": {"type": "array", "items": {"type": "string"}}
                      }
                    }
                  }
                }
              }
            },
            "gate_task": {
              "type": ["object", "null"],
              "additionalProperties": false,
              "required": ["source_id", "title", "dependencies", "size", "gate", "source_status"],
              "properties": {
                "source_id": {"type": "string"},
                "title": {"type": "string"},
                "dependencies": {"type": "array", "items": {"type": "string"}},
                "size": {"type": "string"},
                "gate": {"type": "string"},
                "source_status": {"type": "string"},
                "details_doc": {"type": ["string", "null"]},
                "required_reading": {"type": "array", "items": {"type": "string"}},
                "context_refs": {"type": "array", "items": {"type": "string"}}
              }
            }
          }
        }"#,
    )
    .expect("phase extraction schema should be valid JSON")
}

pub(crate) fn task_extraction_schema() -> Value {
    serde_json::from_str(
        r#"{
          "type": "object",
          "additionalProperties": false,
          "required": ["source_id", "title", "dependencies", "size", "gate", "source_status", "phase_gate"],
          "properties": {
            "source_id": {"type": "string"},
            "title": {"type": "string"},
            "dependencies": {"type": "array", "items": {"type": "string"}},
            "size": {"type": "string"},
            "gate": {"type": "string"},
            "source_status": {"type": "string"},
            "details_doc": {"type": ["string", "null"]},
            "required_reading": {"type": "array", "items": {"type": "string"}},
            "context_refs": {"type": "array", "items": {"type": "string"}},
            "phase_gate": {"type": "boolean"}
          }
        }"#,
    )
    .expect("task extraction schema should be valid JSON")
}

pub(crate) fn build_extraction_prompt(plan_path: &Path, content: &str) -> String {
    format!(
        "You are extracting a software implementation plan into a normalized JSON schema for deterministic import.\n\
Return ONLY valid JSON matching the provided schema. Do not wrap it in markdown. Do not add commentary.\n\
\n\
Extraction rules:\n\
1) Preserve the document title as `title`.\n\
2) Extract top-level execution phases into `phases[]`.\n\
3) Each phase MUST contain at least one `epic`. If the source document has no explicit epic subdivision inside a phase, synthesize exactly one epic with a stable source_id like `<phase>.x` and a title like `Phase <id> work items`.\n\
4) Convert concrete actionable work items into `tasks[]`.\n\
5) Use `gate_task` only for a true phase-level completion gate (for example `Phase N Gate`, `Tests and acceptance for Phase N`, or similarly explicit phase-close validation).\n\
6) Preserve source numbering in `source_id` exactly when the document gives it (examples: `0.1.1`, `4.12`, `4.G`).\n\
7) `dependencies` must list source task ids only. Prefer explicit dependencies from the document. Infer only obvious phase-order or gate dependencies when the document makes the sequencing unambiguous.\n\
8) `source_status` must be one of: `READY`, `BLOCKED`, `IN_PROGRESS`, `DONE`, `FAILED`.\n\
9) Mark `DONE` only when the document explicitly says the item shipped/landed/completed. Mark `IN_PROGRESS` only when the document explicitly says it is current active work. Otherwise prefer `READY` or `BLOCKED`.\n\
10) Keep `gate` concise and testable. Summarize the real acceptance gate in one sentence.\n\
11) If the source names a task-specific packet/details markdown file, preserve it as optional singular `details_doc`.\n\
12) If the source names exact files the worker must inspect before editing, preserve them as optional `required_reading`.\n\
13) If the source names broader background docs or evidence folders, preserve them as optional `context_refs`.\n\
14) Leave `project`, `stack`, and `target` null if the document does not explicitly state them.\n\
\n\
Source file: {}\n\
\n\
Document follows:\n\
<plan_document>\n{}\n</plan_document>",
        plan_path.display(),
        content
    )
}

pub(crate) fn build_phase_extraction_prompt(
    plan_path: &Path,
    document_title: &str,
    status_context: Option<&str>,
    phase: &PhaseExtractionSection,
) -> String {
    let status_block = status_context
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("Document status context:\n<status>\n{value}\n</status>\n\n"))
        .unwrap_or_default();

    format!(
        "You are extracting a single execution phase from a software implementation plan into a normalized JSON schema for deterministic import.\n\
Return ONLY valid JSON matching the provided schema. Do not wrap it in markdown. Do not add commentary.\n\
\n\
Extraction rules:\n\
1) Preserve the phase id and title exactly as shown in the phase heading.\n\
2) Every phase MUST contain at least one `epic`. If the source phase has no explicit epic subdivision, synthesize exactly one epic with source_id `<phase_id>.x` and a concise title like `Phase <id> work items`.\n\
3) Convert concrete actionable work items in this phase into `tasks[]`.\n\
4) Use `gate_task` only for a true phase-level completion gate (for example acceptance, integration, or phase-close validation).\n\
5) Preserve source numbering in `source_id` exactly when the phase gives it (examples: `4.1`, `4.12`, `4.G`).\n\
6) `dependencies` must list source task ids only. Prefer explicit dependencies from the text. Infer only obvious intra-phase dependencies or explicit prior-phase gate dependencies when the section makes sequencing unambiguous.\n\
7) `source_status` must be one of: `READY`, `BLOCKED`, `IN_PROGRESS`, `DONE`, `FAILED`.\n\
8) Use the document status context when it explicitly marks shipped/current work for this phase.\n\
9) Keep `gate` concise and testable. Summarize the real acceptance gate in one sentence.\n\
10) Preserve optional `details_doc` only when the source names one task-specific markdown packet; keep broader supporting docs in optional `context_refs`.\n\
11) Preserve optional `required_reading` only for exact repo-local files the worker must inspect before editing.\n\
12) Notes should capture only short phase-level context that helps execution.\n\
\n\
Source file: {}\n\
Document title: {}\n\
\n\
{}\
Phase section follows:\n\
<phase_section>\n{}\n{}\n</phase_section>",
        plan_path.display(),
        document_title,
        status_block,
        phase.heading,
        phase.body
    )
}

pub(crate) fn build_task_extraction_prompt(
    plan_path: &Path,
    document_title: &str,
    status_context: Option<&str>,
    phase: &PhaseExtractionSection,
    task: &TaskExtractionSection,
    ordered_task_heads: &[String],
) -> String {
    let status_block = status_context
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("Document status context:\n<status>\n{value}\n</status>\n\n"))
        .unwrap_or_default();
    let task_outline = ordered_task_heads.join("\n");

    format!(
        "You are extracting one actionable work item from a software implementation plan into a normalized JSON schema for deterministic import.\n\
Return ONLY valid JSON matching the provided schema. Do not wrap it in markdown. Do not add commentary.\n\
\n\
Extraction rules:\n\
1) Preserve `source_id` and `title` exactly from the task heading.\n\
2) `dependencies` must list source task ids only. Prefer explicit dependencies from the phase text. Infer only obvious dependencies from the phase ordering or gate language.\n\
3) `source_status` must be one of: `READY`, `BLOCKED`, `IN_PROGRESS`, `DONE`, `FAILED`.\n\
4) Use the document status context when it explicitly marks this task shipped or currently active.\n\
5) Estimate `size` coarsely as one of `S`, `M`, `L`, or `XL` from the described scope.\n\
6) Keep `gate` concise and testable. Summarize the acceptance gate in one sentence.\n\
7) Preserve optional `details_doc` only when the task names one task-specific markdown packet; keep broader supporting docs in optional `context_refs`.\n\
8) Preserve optional `required_reading` only for exact repo-local files the worker must inspect before editing.\n\
9) Set `phase_gate=true` only if this task is the phase-level completion gate or acceptance/integration closeout for the phase. Ordinary work items and cleanup tasks must set `phase_gate=false`.\n\
\n\
Source file: {}\n\
Document title: {}\n\
\n\
{}\
Phase heading:\n\
{}\n\
\n\
Ordered task headings for this phase:\n\
<phase_outline>\n{}\n</phase_outline>\n\
\n\
Current task section:\n\
<task_section>\n{}\n{}\n</task_section>",
        plan_path.display(),
        document_title,
        status_block,
        phase.heading,
        task_outline,
        task.heading,
        task.body
    )
}

pub(crate) fn json_from_text_output<T: DeserializeOwned>(output: &str) -> Result<T> {
    if let Ok(value) = serde_json::from_str::<T>(output) {
        return Ok(value);
    }

    if let Ok(value) = serde_json::from_str::<Value>(output) {
        match value {
            Value::Object(_) => {
                return serde_json::from_value(value)
                    .context("Failed to parse extractor JSON object as structured output");
            }
            Value::Array(events) => {
                for event in events.iter().rev() {
                    if let Some(structured) = event.get("structured_output") {
                        return serde_json::from_value(structured.clone())
                            .context("Failed to parse Claude structured_output payload");
                    }

                    if let Some(contents) = event
                        .get("message")
                        .and_then(|message| message.get("content"))
                        .and_then(|content| content.as_array())
                    {
                        for content in contents.iter().rev() {
                            if content.get("type").and_then(|value| value.as_str())
                                != Some("tool_use")
                                || content.get("name").and_then(|value| value.as_str())
                                    != Some("StructuredOutput")
                            {
                                continue;
                            }
                            if let Some(input) = content.get("input") {
                                return serde_json::from_value(input.clone()).context(
                                    "Failed to parse Claude StructuredOutput tool payload",
                                );
                            }
                        }
                    }
                }

                bail!("Extractor JSON output did not contain a structured plan payload");
            }
            _ => {}
        }
    }

    let start = output
        .find('{')
        .ok_or_else(|| anyhow!("Extractor output did not contain a JSON object"))?;
    let end = output
        .rfind('}')
        .ok_or_else(|| anyhow!("Extractor output did not contain a closing JSON object"))?;
    let slice = &output[start..=end];
    serde_json::from_str::<T>(slice)
        .with_context(|| format!("Failed to parse extracted JSON payload: {slice}"))
}

fn summarize_extractor_output(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn extractor_failure_reason(
    launch: &ExtractorLaunch,
    status: std::process::ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    if let Some(stderr_text) = summarize_extractor_output(stderr) {
        return stderr_text;
    }
    if let Some(stdout_text) = summarize_extractor_output(stdout) {
        return format!(
            "extractor exited with status {status} and no stderr; stdout was: {stdout_text}"
        );
    }
    format!(
        "extractor exited with status {status} and produced no stdout/stderr. \
This usually means '{}' failed before returning any structured output.",
        launch.command
    )
}

fn extract_model_from_agent_args(args: &[String]) -> Option<String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--model" || arg == "-m" {
            if let Some(value) = args.get(index + 1) {
                return Some(value.clone());
            }
        }
        if let Some(value) = arg.strip_prefix("model=") {
            return Some(value.trim_matches('"').to_string());
        }
        if let Some(value) = arg.strip_prefix("--model=") {
            return Some(value.to_string());
        }
        index += 1;
    }
    None
}

pub(crate) fn resolve_extractor_launch(
    project_root: &Path,
) -> Result<(ExtractorKind, ExtractorLaunch)> {
    let config = brehon_config::load_config(Some(project_root)).with_context(|| {
        format!(
            "Failed to load Brehon config for '{}'",
            project_root.display()
        )
    })?;
    let supervisor_agent_name = config.roles.supervisor.name.clone();
    let agent = config
        .lane_launcher(&supervisor_agent_name)
        .ok_or_else(|| {
            anyhow!(
                "Supervisor lane '{}' is not defined in config",
                supervisor_agent_name
            )
        })?;
    if agent.adapter != brehon_types::agent::AdapterKind::Acp {
        anyhow::bail!(
            "Supervisor lane '{}' uses adapter {:?}; plan extraction currently requires a subprocess-backed ACP supervisor launcher",
            supervisor_agent_name,
            agent.adapter
        );
    }
    let command_name = agent.command_str().ok_or_else(|| {
        anyhow!(
            "Supervisor lane '{}' has no command configured for subprocess launch",
            supervisor_agent_name
        )
    })?;
    let model = config
        .supervisor
        .model
        .as_ref()
        .map(|model| model.name.clone())
        .or_else(|| {
            config
                .lane_model(&supervisor_agent_name, None)
                .map(|model| model.name.clone())
        })
        .or_else(|| extract_model_from_agent_args(&agent.args));

    let (kind, command, args) = match command_name {
        "claude" => {
            let mut args = vec![
                "-p".to_string(),
                "--permission-mode".to_string(),
                "bypassPermissions".to_string(),
                "--tools".to_string(),
                "".to_string(),
                "--output-format".to_string(),
                "json".to_string(),
                "--no-session-persistence".to_string(),
                "--disable-slash-commands".to_string(),
                "--strict-mcp-config".to_string(),
            ];
            if let Some(model) = model {
                args.push("--model".to_string());
                args.push(model);
            }
            (ExtractorKind::Claude, "claude".to_string(), args)
        }
        "codex" => {
            let mut args = vec![
                "exec".to_string(),
                "--sandbox".to_string(),
                "read-only".to_string(),
                "--skip-git-repo-check".to_string(),
                "--cd".to_string(),
                project_root.display().to_string(),
            ];
            if let Some(model) = model {
                args.push("--model".to_string());
                args.push(model);
            }
            (ExtractorKind::Codex, "codex".to_string(), args)
        }
        "gemini" => {
            let mut args = vec![
                "--prompt".to_string(),
                String::new(),
                "--output-format".to_string(),
                "text".to_string(),
                "--approval-mode".to_string(),
                "plan".to_string(),
                "--sandbox".to_string(),
                "true".to_string(),
            ];
            if let Some(model) = model {
                args.push("--model".to_string());
                args.push(model);
            }
            (ExtractorKind::Gemini, "gemini".to_string(), args)
        }
        "opencode" => {
            let mut args = vec![
                "run".to_string(),
                "--format".to_string(),
                "default".to_string(),
                "--dir".to_string(),
                project_root.display().to_string(),
            ];
            if let Some(model) = model {
                args.push("--model".to_string());
                args.push(model);
            }
            (ExtractorKind::Opencode, "opencode".to_string(), args)
        }
        other => {
            bail!(
                "Supervisor agent '{}' uses unsupported command '{}'. Supported one-shot extractors: claude, codex, gemini, opencode",
                supervisor_agent_name,
                other
            );
        }
    };

    Ok((
        kind,
        ExtractorLaunch {
            agent_name: supervisor_agent_name,
            command,
            args,
            cwd: project_root.to_path_buf(),
        },
    ))
}

async fn run_command_with_optional_stdin(
    mut cmd: Command,
    launch: &ExtractorLaunch,
    stdin_payload: Option<&str>,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    use std::process::Stdio;

    cmd.current_dir(&launch.cwd);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    if stdin_payload.is_some() {
        cmd.stdin(Stdio::piped());
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to launch {}", launch.command))?;

    if let Some(payload) = stdin_payload {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Extractor '{}' did not expose stdin", launch.agent_name))?;
        stdin.write_all(payload.as_bytes()).await.with_context(|| {
            format!(
                "Failed to write prompt to extractor '{}'",
                launch.agent_name
            )
        })?;
        stdin.shutdown().await.with_context(|| {
            format!(
                "Failed to close stdin for extractor '{}'",
                launch.agent_name
            )
        })?;
    }

    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Extractor '{}' did not expose stdout", launch.agent_name))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Extractor '{}' did not expose stderr", launch.agent_name))?;

    // Shared activity clock: every stdout/stderr chunk the reader tasks
    // consume bumps this to `Instant::now()`. The wait loop watches it
    // against the idle threshold. This is the crucial difference vs the
    // old single-wall-clock implementation — a slow-but-progressing
    // extractor on a large plan keeps resetting the idle timer so it
    // isn't punished for taking a long time, while a truly hung
    // extractor (no output at all) still dies inside the idle budget.
    let last_activity = Arc::new(Mutex::new(Instant::now()));
    let stdout_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

    let stdout_task = {
        let last_activity = Arc::clone(&last_activity);
        let buf = Arc::clone(&stdout_buf);
        // stdout is the *structured* extractor output (JSON in most modes,
        // or narration that the caller will parse). Tee'ing it to the
        // user's stdout would corrupt scripts that redirect us. Capture
        // silently.
        tokio::spawn(streaming_reader_task(
            stdout_pipe,
            last_activity,
            buf,
            /* tee_to_stderr */ false,
        ))
    };
    let stderr_task = {
        let last_activity = Arc::clone(&last_activity);
        let buf = Arc::clone(&stderr_buf);
        // stderr is the extractor's progress narration (reasoning
        // tokens, step-by-step log lines). Tee it live to the user's
        // stderr so `brehon extract-plan` shows activity instead of
        // sitting silent for minutes. This is the UX counterpart to the
        // idle timeout — together they make a slow-but-progressing
        // extraction obviously different from a stuck one.
        tokio::spawn(streaming_reader_task(
            stderr_pipe,
            last_activity,
            buf,
            /* tee_to_stderr */ true,
        ))
    };

    let bounds = extractor_bounds();
    let started_at = Instant::now();
    let poll_interval = Duration::from_millis(500);

    let status = loop {
        tokio::select! {
            biased;
            result = child.wait() => {
                break result.with_context(|| {
                    format!("Failed while waiting for extractor '{}'", launch.agent_name)
                })?;
            }
            _ = tokio::time::sleep(poll_interval) => {
                let now = Instant::now();
                let elapsed = now.saturating_duration_since(started_at);
                if elapsed >= bounds.max {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    // Join readers (they'll finish once stdio pipes close)
                    // to surface whatever partial output was captured —
                    // helpful for post-mortem when an extractor sends some
                    // progress narration before stalling.
                    let _ = stdout_task.await;
                    let _ = stderr_task.await;
                    let partial_hint = partial_output_hint(&stdout_buf, &stderr_buf);
                    bail!(
                        "Configured supervisor extractor '{}' exceeded max wall-clock of {}s and was killed. \
                         Tune with BREHON_PLAN_EXTRACT_MAX_TIMEOUT_SECS.{partial_hint}",
                        launch.agent_name,
                        bounds.max.as_secs(),
                    );
                }
                let last_tick = *last_activity.lock().unwrap_or_else(|e| e.into_inner());
                let idle_for = now.saturating_duration_since(last_tick);
                if idle_for >= bounds.idle {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    let _ = stdout_task.await;
                    let _ = stderr_task.await;
                    let partial_hint = partial_output_hint(&stdout_buf, &stderr_buf);
                    bail!(
                        "Configured supervisor extractor '{}' produced no output for {}s (idle limit) after {}s elapsed, and was killed. \
                         Tune with BREHON_PLAN_EXTRACT_IDLE_TIMEOUT_SECS, or raise both via BREHON_PLAN_EXTRACT_TIMEOUT_SECS for legacy semantics.{partial_hint}",
                        launch.agent_name,
                        bounds.idle.as_secs(),
                        elapsed.as_secs(),
                    );
                }
            }
        }
    };

    // Process exited on its own. Drain reader tasks — they've been
    // copying into the shared buffers all along, but we still need to
    // wait for them to see EOF so no tail bytes are lost.
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    let stdout_bytes = std::mem::take(&mut *stdout_buf.lock().unwrap_or_else(|e| e.into_inner()));
    let stderr_bytes = std::mem::take(&mut *stderr_buf.lock().unwrap_or_else(|e| e.into_inner()));

    Ok((status, stdout_bytes, stderr_bytes))
}

/// Reader task used by `run_command_with_optional_stdin`.
///
/// Reads chunks into `buf` and stamps `last_activity` on every non-empty
/// read so the idle-timeout watchdog can distinguish progress from
/// hangs. When `tee_to_stderr` is true the chunk is also written to the
/// parent process's stderr so the operator sees live progress —
/// critical for long extractions where silence looks indistinguishable
/// from a hang.
///
/// I/O errors are propagated by returning `Err`, but the caller
/// intentionally ignores them (via `let _ =`). The process's exit
/// status and the partial buffers carry the signal; a read error on a
/// subprocess pipe is almost always "child exited while we were
/// reading", not a real operator-facing failure.
async fn streaming_reader_task<R>(
    mut reader: R,
    last_activity: Arc<Mutex<Instant>>,
    buf: Arc<Mutex<Vec<u8>>>,
    tee_to_stderr: bool,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use std::io::Write as _;
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => return Ok(()),
            Ok(n) => {
                *last_activity.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
                buf.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend_from_slice(&chunk[..n]);
                if tee_to_stderr {
                    // Best-effort tee. Failing to write to the
                    // operator's stderr (closed pipe, etc.) should
                    // never kill the extraction.
                    let stderr = std::io::stderr();
                    let mut handle = stderr.lock();
                    let _ = handle.write_all(&chunk[..n]);
                    let _ = handle.flush();
                }
            }
            Err(err) => return Err(err),
        }
    }
}

/// Produce a short hint string that lists any stdout/stderr bytes captured
/// before the timeout. Helps operators distinguish "never produced output"
/// (idle timeout, likely credential or auth block) from "produced some
/// output then stalled" (likely upstream API or reasoning-loop issue).
fn partial_output_hint(
    stdout_buf: &Arc<Mutex<Vec<u8>>>,
    stderr_buf: &Arc<Mutex<Vec<u8>>>,
) -> String {
    let stdout_len = stdout_buf.lock().map(|b| b.len()).unwrap_or_default();
    let stderr_len = stderr_buf.lock().map(|b| b.len()).unwrap_or_default();
    if stdout_len == 0 && stderr_len == 0 {
        String::new()
    } else {
        format!(" Captured {stdout_len}B stdout / {stderr_len}B stderr before kill.")
    }
}

async fn run_extractor(
    kind: ExtractorKind,
    launch: &ExtractorLaunch,
    prompt: &str,
    schema: &Value,
) -> Result<String> {
    let mut cmd = Command::new(&launch.command);

    match kind {
        ExtractorKind::Claude => {
            let schema = serde_json::to_string(schema)?;
            let mut args = launch.args.clone();
            args.push("--json-schema".to_string());
            args.push(schema);
            cmd.args(args);
            let (status, stdout, stderr) =
                run_command_with_optional_stdin(cmd, launch, Some(prompt)).await?;
            if !status.success() {
                bail!(
                    "Configured supervisor extractor '{}' failed: {}",
                    launch.agent_name,
                    extractor_failure_reason(launch, status, &stdout, &stderr)
                );
            }
            return Ok(String::from_utf8_lossy(&stdout).to_string());
        }
        ExtractorKind::Codex => {
            let temp_root = launch.cwd.join(".brehon").join("tmp");
            fs::create_dir_all(&temp_root)?;
            let schema_path =
                temp_root.join(format!("plan-extract-schema-{}.json", uuid::Uuid::new_v4()));
            let output_path =
                temp_root.join(format!("plan-extract-output-{}.json", uuid::Uuid::new_v4()));
            fs::write(&schema_path, serde_json::to_string_pretty(schema)?)?;
            let mut args = launch.args.clone();
            args.push("--ephemeral".to_string());
            args.push("--output-schema".to_string());
            args.push(schema_path.display().to_string());
            args.push("--output-last-message".to_string());
            args.push(output_path.display().to_string());
            args.push("-".to_string());
            cmd.args(args);
            let (status, _stdout, stderr) =
                run_command_with_optional_stdin(cmd, launch, Some(prompt)).await?;
            if !status.success() {
                bail!(
                    "Configured supervisor extractor '{}' failed: {}",
                    launch.agent_name,
                    extractor_failure_reason(launch, status, &[], &stderr)
                );
            }
            return fs::read_to_string(&output_path).with_context(|| {
                format!(
                    "Failed to read Codex extraction output '{}'",
                    output_path.display()
                )
            });
        }
        ExtractorKind::Gemini => {
            let mut args = launch.args.clone();
            let prompt_index = args
                .iter()
                .position(|arg| arg.is_empty())
                .ok_or_else(|| anyhow!("Gemini extractor args were malformed"))?;
            args[prompt_index] = String::new();
            cmd.args(args);
            let (status, stdout, stderr) =
                run_command_with_optional_stdin(cmd, launch, Some(prompt)).await?;
            if !status.success() {
                bail!(
                    "Configured supervisor extractor '{}' failed: {}",
                    launch.agent_name,
                    extractor_failure_reason(launch, status, &stdout, &stderr)
                );
            }
            return Ok(String::from_utf8_lossy(&stdout).to_string());
        }
        ExtractorKind::Opencode => {
            let mut args = launch.args.clone();
            args.push(prompt.to_string());
            cmd.args(args);
        }
    }

    let (status, stdout, stderr) = run_command_with_optional_stdin(cmd, launch, None).await?;
    if !status.success() {
        bail!(
            "Configured supervisor extractor '{}' failed: {}",
            launch.agent_name,
            extractor_failure_reason(launch, status, &stdout, &stderr)
        );
    }
    Ok(String::from_utf8_lossy(&stdout).to_string())
}

pub(crate) async fn extract_document_with_supervisor(
    project_root: &Path,
    plan_path: &Path,
) -> Result<PlanDocument> {
    let content = fs::read_to_string(plan_path)
        .with_context(|| format!("Failed to read plan file '{}'", plan_path.display()))?;
    let (kind, launch) = resolve_extractor_launch(project_root)?;
    let bounds = extractor_bounds();
    eprintln!(
        "Using supervisor extractor '{}' via '{}' (idle timeout {}s, max wall-clock {}s).",
        launch.agent_name,
        launch.command,
        bounds.idle.as_secs(),
        bounds.max.as_secs(),
    );
    if let Some(chunked) = parse_chunkable_plan_document(plan_path, &content)? {
        let total = chunked.phases.len();
        let mut phases = Vec::with_capacity(total);
        for (index, phase) in chunked.phases.iter().enumerate() {
            let task_sections = parse_task_extraction_sections(phase);
            if !task_sections.is_empty() {
                eprintln!(
                    "Extracting phase {}/{} as {} task-sized chunks: Phase {} — {}",
                    index + 1,
                    total,
                    task_sections.len(),
                    phase.id,
                    phase.title
                );
                let ordered_headings = task_sections
                    .iter()
                    .map(|task| task.heading.clone())
                    .collect::<Vec<_>>();
                let mut extracted_tasks = Vec::with_capacity(task_sections.len());
                for (task_index, task) in task_sections.iter().enumerate() {
                    eprintln!(
                        "  Extracting task {}/{}: {} {}",
                        task_index + 1,
                        task_sections.len(),
                        task.source_id,
                        task.title
                    );
                    let prompt = build_task_extraction_prompt(
                        plan_path,
                        &chunked.title,
                        chunked.status_context.as_deref(),
                        phase,
                        task,
                        &ordered_headings,
                    );
                    let raw =
                        run_extractor(kind.clone(), &launch, &prompt, &task_extraction_schema())
                            .await?;
                    let extracted: ExtractedTaskSection = json_from_text_output(&raw)?;
                    if extracted.source_id.trim() != task.source_id
                        || !extracted_metadata_matches(&task.title, &extracted.title)
                    {
                        bail!(
                            "Extractor returned mismatched task metadata for {}. Expected '{} / {}', got '{} / {}'",
                            task.source_id,
                            task.source_id,
                            task.title,
                            extracted.source_id,
                            extracted.title
                        );
                    }
                    extracted_tasks.push(extracted);
                }

                let mut gate_candidates = extracted_tasks
                    .iter()
                    .filter(|task| task.phase_gate)
                    .cloned()
                    .collect::<Vec<_>>();
                if gate_candidates.is_empty() {
                    gate_candidates = extracted_tasks
                        .iter()
                        .filter(|task| {
                            task.title
                                .to_ascii_lowercase()
                                .contains("tests and acceptance")
                                || task.title.to_ascii_lowercase().contains("phase gate")
                        })
                        .cloned()
                        .collect();
                }
                let gate_task = gate_candidates.first().map(|task| PlanTask {
                    source_id: task.source_id.clone(),
                    title: task.title.clone(),
                    dependencies: task.dependencies.clone(),
                    size: task.size.clone(),
                    gate: task.gate.clone(),
                    source_status: task.source_status.clone(),
                    details_doc: task.details_doc.clone(),
                    required_reading: task.required_reading.clone(),
                    context_refs: task.context_refs.clone(),
                });
                let gate_source_id = gate_task.as_ref().map(|task| task.source_id.clone());
                let tasks = extracted_tasks
                    .into_iter()
                    .filter(|task| Some(task.source_id.clone()) != gate_source_id)
                    .map(|task| PlanTask {
                        source_id: task.source_id,
                        title: task.title,
                        dependencies: task.dependencies,
                        size: task.size,
                        gate: task.gate,
                        source_status: task.source_status,
                        details_doc: task.details_doc,
                        required_reading: task.required_reading,
                        context_refs: task.context_refs,
                    })
                    .collect::<Vec<_>>();

                phases.push(PlanPhase {
                    id: phase.id.clone(),
                    title: phase.title.clone(),
                    notes: vec!["Imported from prose phase section".to_string()],
                    epics: vec![PlanEpic {
                        source_id: format!("{}.x", phase.id),
                        title: format!("Phase {} work items", phase.id),
                        tasks,
                    }],
                    gate_task,
                });
                continue;
            }

            eprintln!(
                "Extracting phase {}/{}: Phase {} — {}",
                index + 1,
                total,
                phase.id,
                phase.title
            );
            let prompt = build_phase_extraction_prompt(
                plan_path,
                &chunked.title,
                chunked.status_context.as_deref(),
                phase,
            );
            let raw =
                run_extractor(kind.clone(), &launch, &prompt, &phase_extraction_schema()).await?;
            let mut extracted: PlanPhase = json_from_text_output(&raw)?;
            if !extracted_phase_id_matches(&phase.id, &extracted.id)
                || !extracted_metadata_matches(&phase.title, &extracted.title)
            {
                bail!(
                    "Extractor returned mismatched phase metadata for phase {}. Expected '{} / {}', got '{} / {}'",
                    phase.id,
                    phase.id,
                    phase.title,
                    extracted.id,
                    extracted.title
                );
            }
            extracted.id = phase.id.clone();
            extracted.title = phase.title.clone();
            if extracted.notes.is_empty() && !phase.body.is_empty() {
                extracted
                    .notes
                    .push("Imported from prose phase section".to_string());
            }
            phases.push(extracted);
        }

        return validate_plan_document(
            &PlanDocument {
                title: chunked.title,
                project: chunked.project,
                stack: chunked.stack,
                target: chunked.target,
                path: plan_path.to_path_buf(),
                already_landed_commit: None,
                phases,
            },
            plan_path,
        );
    }

    eprintln!(
        "Extracting full document through supervisor extractor for '{}'.",
        plan_path.display()
    );
    let prompt = build_extraction_prompt(plan_path, &content);
    let raw = run_extractor(kind, &launch, &prompt, &extraction_schema()).await?;
    let mut plan: PlanDocument = json_from_text_output(&raw)?;
    if plan.path.as_os_str().is_empty() {
        plan.path = plan_path.to_path_buf();
    }
    validate_plan_document(&plan, plan_path)
}

pub(crate) async fn load_plan_document(
    project_root: &Path,
    plan_path: &Path,
    mode: ExtractMode,
) -> Result<PlanDocument> {
    let extension = plan_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());

    if matches!(extension.as_deref(), Some("json")) {
        return parse_normalized_plan(plan_path);
    }

    match mode {
        ExtractMode::Direct => {
            let plan = parse_document(plan_path)?;
            validate_plan_document(&plan, plan_path)
        }
        ExtractMode::Supervisor => extract_document_with_supervisor(project_root, plan_path).await,
        ExtractMode::Auto => match parse_document(plan_path) {
            Ok(plan) => validate_plan_document(&plan, plan_path),
            Err(parse_err) => {
                eprintln!(
                    "Direct markdown import failed ({}). Falling back to supervisor extraction...",
                    parse_err
                );
                extract_document_with_supervisor(project_root, plan_path).await
            }
        },
    }
}

#[cfg(test)]
mod timeout_resolution_tests {
    use super::*;

    // All tests in this module mutate env vars, so they must run under
    // this lock. Without it, `cargo test` concurrency will produce flaky
    // cross-test contamination.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct ScopedEnv {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }
    impl ScopedEnv {
        fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
            let mut saved = Vec::with_capacity(vars.len());
            for (key, value) in vars {
                saved.push((*key, std::env::var_os(key)));
                // Rust 2024: env mutation is unsafe. The test-module
                // lock above serializes all access so the safety
                // invariant (no concurrent env races) holds.
                unsafe {
                    match value {
                        Some(v) => std::env::set_var(key, v),
                        None => std::env::remove_var(key),
                    }
                }
            }
            Self { saved }
        }
    }
    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            for (key, value) in self.saved.iter().rev() {
                unsafe {
                    match value {
                        Some(v) => std::env::set_var(key, v),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    const ALL_VARS: [(&str, Option<&str>); 3] = [
        ("BREHON_PLAN_EXTRACT_TIMEOUT_SECS", None),
        ("BREHON_PLAN_EXTRACT_IDLE_TIMEOUT_SECS", None),
        ("BREHON_PLAN_EXTRACT_MAX_TIMEOUT_SECS", None),
    ];

    #[test]
    fn defaults_when_no_env_vars_are_set() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = ScopedEnv::set(&ALL_VARS);
        let bounds = extractor_bounds();
        assert_eq!(bounds.idle.as_secs(), DEFAULT_EXTRACTOR_IDLE_TIMEOUT_SECS);
        assert_eq!(bounds.max.as_secs(), DEFAULT_EXTRACTOR_MAX_TIMEOUT_SECS);
    }

    #[test]
    fn legacy_timeout_var_sets_both_idle_and_max() {
        // Users who set `BREHON_PLAN_EXTRACT_TIMEOUT_SECS` on the old
        // single-wall-clock contract must keep getting exactly that:
        // if they set 600, the extractor can't run longer than 600 s
        // AND any 600-second silence kills it. This preserves the
        // pre-split behavior bit-for-bit.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = ScopedEnv::set(&[
            ("BREHON_PLAN_EXTRACT_TIMEOUT_SECS", Some("600")),
            ("BREHON_PLAN_EXTRACT_IDLE_TIMEOUT_SECS", None),
            ("BREHON_PLAN_EXTRACT_MAX_TIMEOUT_SECS", None),
        ]);
        let bounds = extractor_bounds();
        assert_eq!(bounds.idle.as_secs(), 600);
        assert_eq!(bounds.max.as_secs(), 600);
    }

    #[test]
    fn explicit_vars_override_legacy() {
        // When both the legacy and the new vars are set, the new vars
        // win for their specific dimension.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = ScopedEnv::set(&[
            ("BREHON_PLAN_EXTRACT_TIMEOUT_SECS", Some("600")),
            ("BREHON_PLAN_EXTRACT_IDLE_TIMEOUT_SECS", Some("300")),
            ("BREHON_PLAN_EXTRACT_MAX_TIMEOUT_SECS", Some("3600")),
        ]);
        let bounds = extractor_bounds();
        assert_eq!(bounds.idle.as_secs(), 300);
        assert_eq!(bounds.max.as_secs(), 3600);
    }

    #[test]
    fn idle_is_clamped_to_max_when_configured_higher() {
        // An idle timeout longer than max is meaningless — the max
        // would fire first. Clamp so operators can't create that
        // nonsensical configuration.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = ScopedEnv::set(&[
            ("BREHON_PLAN_EXTRACT_TIMEOUT_SECS", None),
            ("BREHON_PLAN_EXTRACT_IDLE_TIMEOUT_SECS", Some("900")),
            ("BREHON_PLAN_EXTRACT_MAX_TIMEOUT_SECS", Some("300")),
        ]);
        let bounds = extractor_bounds();
        assert_eq!(bounds.idle.as_secs(), 300);
        assert_eq!(bounds.max.as_secs(), 300);
    }

    #[test]
    fn zero_or_invalid_env_values_fall_back_to_defaults() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = ScopedEnv::set(&[
            ("BREHON_PLAN_EXTRACT_TIMEOUT_SECS", Some("0")),
            (
                "BREHON_PLAN_EXTRACT_IDLE_TIMEOUT_SECS",
                Some("not-a-number"),
            ),
            ("BREHON_PLAN_EXTRACT_MAX_TIMEOUT_SECS", Some("")),
        ]);
        let bounds = extractor_bounds();
        assert_eq!(bounds.idle.as_secs(), DEFAULT_EXTRACTOR_IDLE_TIMEOUT_SECS);
        assert_eq!(bounds.max.as_secs(), DEFAULT_EXTRACTOR_MAX_TIMEOUT_SECS);
    }

    #[test]
    fn only_idle_var_set_leaves_max_at_default() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = ScopedEnv::set(&[
            ("BREHON_PLAN_EXTRACT_TIMEOUT_SECS", None),
            ("BREHON_PLAN_EXTRACT_IDLE_TIMEOUT_SECS", Some("60")),
            ("BREHON_PLAN_EXTRACT_MAX_TIMEOUT_SECS", None),
        ]);
        let bounds = extractor_bounds();
        assert_eq!(bounds.idle.as_secs(), 60);
        assert_eq!(bounds.max.as_secs(), DEFAULT_EXTRACTOR_MAX_TIMEOUT_SECS);
    }

    #[test]
    fn only_max_var_set_leaves_idle_at_default() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = ScopedEnv::set(&[
            ("BREHON_PLAN_EXTRACT_TIMEOUT_SECS", None),
            ("BREHON_PLAN_EXTRACT_IDLE_TIMEOUT_SECS", None),
            ("BREHON_PLAN_EXTRACT_MAX_TIMEOUT_SECS", Some("7200")),
        ]);
        let bounds = extractor_bounds();
        assert_eq!(bounds.idle.as_secs(), DEFAULT_EXTRACTOR_IDLE_TIMEOUT_SECS);
        assert_eq!(bounds.max.as_secs(), 7200);
    }
}

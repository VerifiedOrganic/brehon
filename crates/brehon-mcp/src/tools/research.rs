//! Research agent tool.
//!
//! Research jobs are deliberately persistence-only. Creating a job records a
//! prompt and optional routing metadata, then returns immediately. Research
//! agents claim queued jobs out-of-band and submit append-only artifacts that
//! workers and reviewers receive as compact task context.

#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use brehon_types::config::ContextCompressionTarget;
use brehon_types::{
    BrehonConfig, ResearchConfig, ResearchJobTemplateConfig, ResearchOutputSchema,
    ResearchPoolConfig, ResearchRouteConfig, ResearchRouteMatchConfig, ResearchTrigger,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::McpError;
use crate::server::ToolResult;
use crate::tools::agent::{
    resolve_supervisor_name, session_is_live, session_matches_current_runtime, try_deliver_message,
};
use crate::tools::context_efficiency::{
    compact_model_context_with_notice, load_context_tool_options,
};
use crate::tools::{error_result, text_result, Tool};

const JOB_STATUS_QUEUED: &str = "queued";
const JOB_STATUS_RUNNING: &str = "running";
const JOB_STATUS_COMPLETED: &str = "completed";
const JOB_STATUS_FAILED: &str = "failed";
const JOB_STATUS_ARCHIVED: &str = "archived";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ResearchContextEntry {
    pub artifact_id: String,
    pub job_id: String,
    pub pool: String,
    pub role: String,
    pub title: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    pub artifact_path: String,
    pub structured_path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub citations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handoff_deliveries: Vec<ResearchHandoffDelivery>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handoff_warnings: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ResearchHandoffDelivery {
    pub target: String,
    pub target_role: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub queued_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResearchJobRecord {
    job_id: String,
    task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    route_id: Option<String>,
    template_id: String,
    pool: String,
    lane: String,
    role: String,
    status: String,
    origin: String,
    prompt: String,
    cost_units: u32,
    requested_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assigned_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artifact_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResearchManifest {
    task_id: String,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    artifacts: Vec<ResearchContextEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResearchGlobalManifest {
    updated_at: DateTime<Utc>,
    #[serde(default)]
    tasks: Vec<ResearchTaskManifestSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResearchTaskManifestSummary {
    task_id: String,
    artifact_count: usize,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StructuredResearchArtifact {
    schema: String,
    title: String,
    summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    findings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    citations: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    confidence: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    validation_warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct QueuedJob {
    record: ResearchJobRecord,
    notified_agents: Vec<String>,
}

/// MCP tool for read-only research jobs and artifacts.
pub struct ResearchTool;

impl Default for ResearchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ResearchTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ResearchTool {
    fn name(&self) -> &str {
        "research"
    }

    fn description(&self) -> &str {
        "Persistent, non-blocking research jobs and append-only task artifacts. \
         Actions: status, list, get, request, run_route, claim_next, submit, attach, detach, archive."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "status, list, get, request, run_route, claim_next, submit, attach, detach, or archive"
                },
                "task_id": {"type": "string"},
                "job_id": {"type": "string"},
                "artifact_id": {"type": "string"},
                "route_id": {"type": "string"},
                "trigger": {"type": "string", "description": "before_assignment, before_review, or manual"},
                "pool": {"type": "string", "description": "Research pool id"},
                "role": {"type": "string", "description": "Research role such as normative_requirements"},
                "prompt": {"type": "string", "description": "Research prompt for action=request"},
                "title": {"type": "string"},
                "summary": {"type": "string"},
                "confidence": {"type": "string"},
                "content": {"type": "string", "description": "Markdown brief for action=submit"},
                "structured": {"type": "object", "description": "Structured artifact payload for action=submit"},
                "findings": {"type": "array", "items": {"type": "string"}},
                "citations": {"type": "array", "items": {"type": "string"}},
                "supersedes": {"type": "array", "items": {"type": "string"}},
                "force": {"type": "boolean", "description": "Allow duplicate automatic route jobs"}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, McpError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        if caller_role() == "research"
            && !matches!(action, "status" | "list" | "get" | "claim_next" | "submit")
        {
            return Ok(error_result(format!(
                "Research agents may only use research actions status, list, get, claim_next, and submit; action '{action}' is not allowed."
            )));
        }
        let result = match action {
            "status" => research_status(),
            "list" => list_research(&args),
            "get" => get_research(&args),
            "request" => request_research(&args),
            "run_route" => run_route_action(&args),
            "claim_next" => claim_next(&args),
            "submit" => submit_artifact(&args),
            "attach" => attach_action(&args),
            "detach" => detach_action(&args),
            "archive" => archive_action(&args),
            "" => Err("missing research action".to_string()),
            other => Err(format!("unknown research action '{other}'")),
        };

        match result {
            Ok(value) => Ok(text_result(
                serde_json::to_string_pretty(&value)
                    .map_err(|err| McpError::Serialization(err.to_string()))?,
            )),
            Err(message) => Ok(error_result(message)),
        }
    }
}

pub(crate) fn run_automatic_routes_for_task(
    task_id: &str,
    trigger: ResearchTrigger,
    requested_by: &str,
) -> Result<Vec<Value>, String> {
    let config = load_project_config()?;
    if !config.research.enabled {
        return Ok(Vec::new());
    }
    let task = read_task(task_id).ok_or_else(|| format!("task '{task_id}' not found"))?;
    let queued = queue_matching_routes(&config, &task, trigger, requested_by, false)?;
    Ok(queued
        .into_iter()
        .map(|queued| {
            serde_json::json!({
                "job_id": queued.record.job_id,
                "task_id": queued.record.task_id,
                "pool": queued.record.pool,
                "role": queued.record.role,
                "status": queued.record.status,
                "notified_agents": queued.notified_agents,
            })
        })
        .collect())
}

pub(crate) fn render_task_research_handoff(task: &Value, config: Option<&BrehonConfig>) -> String {
    let max_entries = config
        .map(|config| config.research.attach.max_attached_artifacts)
        .unwrap_or(6);
    let include_summaries = config
        .map(|config| config.research.attach.include_summaries)
        .unwrap_or(true);
    let include_manifest = config
        .map(|config| config.research.attach.include_manifest)
        .unwrap_or(true);

    let Some(entries) = task.get("research_context").and_then(Value::as_array) else {
        return String::new();
    };
    if entries.is_empty() {
        return String::new();
    }

    let mut out = String::from("Research context available for this task:\n");
    for entry in entries.iter().rev().take(max_entries).rev() {
        let artifact_id = string_field(entry, "artifact_id").unwrap_or("unknown");
        let role = string_field(entry, "role").unwrap_or("research");
        let title = string_field(entry, "title").unwrap_or("Untitled research artifact");
        let path = string_field(entry, "artifact_path").unwrap_or("");
        out.push_str(&format!("- {artifact_id} [{role}] {title}"));
        if !path.is_empty() {
            out.push_str(&format!(" ({path})"));
        }
        out.push('\n');
        if include_summaries {
            if let Some(summary) = string_field(entry, "summary") {
                out.push_str("  ");
                out.push_str(&compact_line(summary, 320));
                out.push('\n');
            }
        }
    }
    if include_manifest {
        if let Some(task_id) = string_field(task, "task_id") {
            out.push_str(&format!(
                "Manifest: .brehon/runtime/research/{}/manifest.yaml\n",
                sanitize_id(task_id)
            ));
        }
    }
    out.push_str(
        "Treat research artifacts as context, not proof. Verify any claim against source files, specs, or the current diff before relying on it.\n",
    );
    out
}

fn research_status() -> Result<Value, String> {
    let config = load_project_config().ok();
    let jobs = read_all_jobs()?;
    let mut by_status: HashMap<String, usize> = HashMap::new();
    for job in &jobs {
        *by_status.entry(job.status.clone()).or_default() += 1;
    }
    let configured = config.as_ref().map(|config| {
        serde_json::json!({
            "enabled": config.research.enabled,
            "artifact_root": config.research.artifact_root,
            "defaults": config.research.defaults,
            "worker_requests": config.research.worker_requests,
            "pools": config.research.pools.iter().map(|pool| {
                serde_json::json!({
                    "id": pool.id,
                    "lane": pool.lane,
                    "role": pool.role,
                    "min": pool.min,
                    "max": pool.max,
                    "cost_units": pool.cost_units,
                    "permissions": pool.permissions,
                    "output_schema": pool.output_schema,
                })
            }).collect::<Vec<_>>(),
            "routes": config.research.routes.iter().map(|route| {
                serde_json::json!({
                    "id": route.id,
                    "enabled": route.enabled,
                    "trigger": route.trigger,
                    "jobs": route.jobs.iter().map(|job| job.id.clone()).collect::<Vec<_>>(),
                })
            }).collect::<Vec<_>>(),
        })
    });
    Ok(serde_json::json!({
        "status": "ok",
        "configured": configured,
        "jobs_by_status": by_status,
        "job_count": jobs.len(),
        "next_action": "Research jobs are async. Use research action=claim_next from a research agent, or action=request to queue a job without blocking task execution."
    }))
}

fn list_research(args: &Value) -> Result<Value, String> {
    let task_id = string_arg(args, "task_id");
    let mut jobs = read_all_jobs()?;
    if let Some(task_id) = task_id.as_deref() {
        jobs.retain(|job| job.task_id == task_id);
    }
    jobs.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left.job_id.cmp(&right.job_id))
    });

    let manifests = if let Some(task_id) = task_id.as_deref() {
        vec![read_manifest(task_id)?]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
    } else {
        read_all_manifests()?
    };

    Ok(serde_json::json!({
        "jobs": jobs,
        "manifests": manifests,
    }))
}

fn get_research(args: &Value) -> Result<Value, String> {
    let task_id = required_string_arg(args, "task_id")?;
    if let Some(job_id) = string_arg(args, "job_id") {
        let job = read_job(&task_id, &job_id)?
            .ok_or_else(|| format!("research job '{job_id}' not found for task '{task_id}'"))?;
        return Ok(serde_json::json!({ "job": job }));
    }
    if let Some(artifact_id) = string_arg(args, "artifact_id") {
        let manifest = read_manifest(&task_id)?
            .ok_or_else(|| format!("research manifest not found for task '{task_id}'"))?;
        let entry = manifest
            .artifacts
            .iter()
            .find(|entry| entry.artifact_id == artifact_id)
            .ok_or_else(|| {
                format!("research artifact '{artifact_id}' not found for task '{task_id}'")
            })?;
        let brief_path = artifact_path(&task_id, &artifact_id, "brief.md")?;
        let structured_path = artifact_path(&task_id, &artifact_id, "artifact.yaml")?;
        let brief = std::fs::read_to_string(&brief_path).unwrap_or_default();
        let structured = std::fs::read_to_string(&structured_path).unwrap_or_default();
        return Ok(serde_json::json!({
            "artifact": entry,
            "brief": brief,
            "structured": structured,
        }));
    }
    Err("research action=get requires job_id or artifact_id".to_string())
}

fn request_research(args: &Value) -> Result<Value, String> {
    let config = load_project_config()?;
    if !config.research.enabled {
        return Err("research.enabled is false; no research jobs can be queued".to_string());
    }
    let task_id = required_string_arg(args, "task_id")?;
    let prompt = required_string_arg(args, "prompt")?;
    let task = read_task(&task_id).ok_or_else(|| format!("task '{task_id}' not found"))?;

    let pool = resolve_request_pool(&config.research, args)?;
    enforce_worker_request_limits(&config.research, &task_id, pool)?;
    let template = ResearchJobTemplateConfig {
        pool: pool.id.clone(),
        id: string_arg(args, "role").unwrap_or_else(|| pool.role.clone()),
        depends_on: Vec::new(),
        prompt_template: prompt,
    };
    let requested_by = caller_name();
    let origin = if caller_role() == "worker" {
        "worker_request"
    } else {
        "manual_request"
    };
    let queued = queue_job(
        &config,
        &task,
        None,
        &template,
        pool,
        origin,
        &requested_by,
        true,
    )?;

    Ok(serde_json::json!({
        "status": "queued",
        "job": queued.record,
        "notified_agents": queued.notified_agents,
        "next_action": "Do not wait synchronously. Research agents will claim this job and attach artifacts when ready."
    }))
}

fn run_route_action(args: &Value) -> Result<Value, String> {
    let config = load_project_config()?;
    if !config.research.enabled {
        return Err("research.enabled is false; no research routes can be queued".to_string());
    }
    let task_id = required_string_arg(args, "task_id")?;
    let trigger = string_arg(args, "trigger")
        .as_deref()
        .map(parse_trigger)
        .transpose()?
        .unwrap_or(ResearchTrigger::Manual);
    let force = bool_arg(args, "force");
    let task = read_task(&task_id).ok_or_else(|| format!("task '{task_id}' not found"))?;

    let queued = if let Some(route_id) = string_arg(args, "route_id") {
        let route = config
            .research
            .routes
            .iter()
            .find(|route| route.id == route_id)
            .ok_or_else(|| format!("research route '{route_id}' is not configured"))?;
        queue_route(&config, &task, route, &caller_name(), force)?
    } else {
        queue_matching_routes(&config, &task, trigger, &caller_name(), force)?
    };

    Ok(serde_json::json!({
        "status": "ok",
        "queued": queued.into_iter().map(|queued| {
            serde_json::json!({
                "job": queued.record,
                "notified_agents": queued.notified_agents,
            })
        }).collect::<Vec<_>>(),
        "next_action": "Routes only enqueue research jobs. Task execution continues even if no research agent is available."
    }))
}

fn claim_next(args: &Value) -> Result<Value, String> {
    let config = load_project_config().ok();
    let pool_filter = string_arg(args, "pool").or_else(|| {
        std::env::var("BREHON_AGENT_TYPE")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .and_then(|lane| {
                config.as_ref().and_then(|config| {
                    config
                        .research
                        .pools
                        .iter()
                        .find(|pool| pool.lane == lane || pool.id == lane)
                        .map(|pool| pool.id.clone())
                })
            })
    });
    let role_filter = string_arg(args, "role");
    let task_filter = string_arg(args, "task_id");
    let agent = caller_name();

    let all_jobs = read_all_jobs()?;
    if let Some(job) = all_jobs
        .iter()
        .filter(|job| {
            job.status == JOB_STATUS_RUNNING
                && job.assigned_to.as_deref() == Some(agent.as_str())
                && job_matches_claim_filters(
                    job,
                    pool_filter.as_deref(),
                    role_filter.as_deref(),
                    task_filter.as_deref(),
                )
        })
        .min_by(|left, right| {
            left.updated_at
                .cmp(&right.updated_at)
                .then_with(|| left.job_id.cmp(&right.job_id))
        })
        .cloned()
    {
        return Ok(serde_json::json!({
            "status": "claimed",
            "message": "this research agent already has a running job; resume and submit it before claiming another",
            "job": job,
            "submit": research_submit_hint(),
            "next": research_next_claim_hint(),
        }));
    }

    let mut jobs = all_jobs;
    jobs.retain(|job| job.status == JOB_STATUS_QUEUED);
    if let Some(pool) = pool_filter.as_deref() {
        jobs.retain(|job| job.pool == pool);
    }
    if let Some(role) = role_filter.as_deref() {
        jobs.retain(|job| job.role == role);
    }
    if let Some(task_id) = task_filter.as_deref() {
        jobs.retain(|job| job.task_id == task_id);
    }
    jobs.retain(|job| dependencies_satisfied(job).unwrap_or(false));
    jobs.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.job_id.cmp(&right.job_id))
    });

    if let Some(config) = config.as_ref().filter(|config| config.research.enabled) {
        let running_total = running_jobs(None)?;
        let max_parallel = config.research.defaults.max_parallel_jobs as usize;
        if running_total >= max_parallel {
            return Ok(serde_json::json!({
                "status": "idle",
                "message": format!("research concurrency limit reached: {running_total}/{max_parallel} jobs are already running"),
            }));
        }

        let had_capacity_candidates = !jobs.is_empty();
        let mut running_by_pool: HashMap<String, usize> = HashMap::new();
        let mut capacity_filtered = Vec::new();
        for job in jobs {
            let Some(pool) = config
                .research
                .pools
                .iter()
                .find(|pool| pool.id == job.pool)
            else {
                continue;
            };
            let running_for_pool = match running_by_pool.get(&job.pool) {
                Some(count) => *count,
                None => {
                    let count = running_jobs(Some(&job.pool))?;
                    running_by_pool.insert(job.pool.clone(), count);
                    count
                }
            };
            if running_for_pool < pool.max as usize {
                capacity_filtered.push(job);
            }
        }
        jobs = capacity_filtered;
        if had_capacity_candidates && jobs.is_empty() {
            return Ok(serde_json::json!({
                "status": "idle",
                "message": "no queued research jobs have available pool capacity",
            }));
        }
    }

    let Some(mut job) = jobs.into_iter().next() else {
        return Ok(serde_json::json!({
            "status": "idle",
            "message": "no queued research jobs match this agent",
        }));
    };

    job.status = JOB_STATUS_RUNNING.to_string();
    job.assigned_to = Some(agent);
    job.updated_at = Utc::now();
    write_job(&job)?;

    Ok(serde_json::json!({
        "status": "claimed",
        "job": job,
        "submit": research_submit_hint(),
        "next": research_next_claim_hint(),
    }))
}

fn research_submit_hint() -> &'static str {
    "When complete, call research action=submit task_id=<task_id> job_id=<job_id> summary=<summary> content=<markdown brief> citations=[...]"
}

fn research_next_claim_hint() -> &'static str {
    "After submit succeeds, call research action=claim_next again and continue one job at a time until it returns idle."
}

fn job_matches_claim_filters(
    job: &ResearchJobRecord,
    pool: Option<&str>,
    role: Option<&str>,
    task_id: Option<&str>,
) -> bool {
    pool.is_none_or(|pool| job.pool == pool)
        && role.is_none_or(|role| job.role == role)
        && task_id.is_none_or(|task_id| job.task_id == task_id)
}

fn submit_artifact(args: &Value) -> Result<Value, String> {
    let config = load_project_config().ok();
    let task_id = required_string_arg(args, "task_id")?;
    let job_id = required_string_arg(args, "job_id")?;
    let mut job = read_job(&task_id, &job_id)?
        .ok_or_else(|| format!("research job '{job_id}' not found for task '{task_id}'"))?;
    if matches!(
        job.status.as_str(),
        JOB_STATUS_COMPLETED | JOB_STATUS_ARCHIVED
    ) {
        return Err(format!(
            "research job '{job_id}' is already {}; refusing duplicate submit",
            job.status
        ));
    }

    let title = string_arg(args, "title").unwrap_or_else(|| job.template_id.clone());
    let summary = required_string_arg(args, "summary")?;
    let citations = string_array_arg(args, "citations");
    let findings = string_array_arg(args, "findings");
    let confidence = string_arg(args, "confidence");
    let supersedes = string_array_arg(args, "supersedes");
    let artifact_id = next_artifact_id(&task_id, &job.template_id)?;
    let schema = config
        .as_ref()
        .and_then(|config| {
            config
                .research
                .pools
                .iter()
                .find(|pool| pool.id == job.pool)
        })
        .map(|pool| pool.output_schema)
        .unwrap_or(ResearchOutputSchema::SpecBrief);
    let require_citations = config
        .as_ref()
        .map(|config| config.research.defaults.require_citations)
        .unwrap_or(false);
    let mut validation_warnings = Vec::new();
    if require_citations && citations.is_empty() {
        validation_warnings.push(
            "research.defaults.require_citations=true but artifact has no citations".to_string(),
        );
    }

    let structured = structured_arg(args).unwrap_or_else(|| StructuredResearchArtifact {
        schema: schema.as_str().to_string(),
        title: title.clone(),
        summary: summary.clone(),
        findings: findings.clone(),
        citations: citations.clone(),
        confidence: confidence.clone(),
        validation_warnings: validation_warnings.clone(),
    });
    let brief = string_arg(args, "content").unwrap_or_else(|| render_brief(&structured));
    let structured_yaml = serde_yaml::to_string(&structured)
        .map_err(|err| format!("failed to serialize structured artifact: {err}"))?;
    let max_artifact_bytes = config
        .as_ref()
        .map(|config| config.research.defaults.max_artifact_bytes)
        .unwrap_or(200_000);
    let artifact_bytes = brief.len().saturating_add(structured_yaml.len());
    if artifact_bytes as u64 > max_artifact_bytes {
        return Err(format!(
            "research artifact is {artifact_bytes} bytes, above max_artifact_bytes={max_artifact_bytes}"
        ));
    }

    let artifact_dir = artifact_dir(&task_id, &artifact_id)?;
    std::fs::create_dir_all(&artifact_dir).map_err(|err| {
        format!(
            "failed to create artifact dir {}: {err}",
            artifact_dir.display()
        )
    })?;
    let brief_path = artifact_dir.join("brief.md");
    let structured_path = artifact_dir.join("artifact.yaml");
    atomic_write(&brief_path, brief.as_bytes())?;
    atomic_write(&structured_path, structured_yaml.as_bytes())?;

    let task_snapshot = read_task(&task_id).ok_or_else(|| format!("task '{task_id}' not found"))?;
    let mut entry = ResearchContextEntry {
        artifact_id: artifact_id.clone(),
        job_id: job.job_id.clone(),
        pool: job.pool.clone(),
        role: job.role.clone(),
        title,
        summary,
        confidence,
        artifact_path: project_relative_path(&brief_path),
        structured_path: project_relative_path(&structured_path),
        citations,
        supersedes,
        handoff_deliveries: Vec::new(),
        handoff_warnings: Vec::new(),
        created_at: Utc::now(),
    };
    let handoff = deliver_research_handoff(&task_id, &task_snapshot, &entry);
    entry.handoff_deliveries = handoff.deliveries;
    entry.handoff_warnings = handoff.warnings;

    append_manifest_entry(&task_id, entry.clone())?;
    attach_artifact_to_task(&task_id, &entry)?;

    job.status = JOB_STATUS_COMPLETED.to_string();
    job.artifact_id = Some(artifact_id.clone());
    job.warnings.extend(validation_warnings);
    job.warnings.extend(entry.handoff_warnings.clone());
    job.updated_at = Utc::now();
    write_job(&job)?;

    let mut result = serde_json::json!({
        "status": "ok",
        "artifact": entry,
        "job": job,
        "next_action": "Artifact attached to task.research_context. Workers/reviewers should treat it as advisory context and verify claims."
    });
    if !result["artifact"]["handoff_deliveries"]
        .as_array()
        .is_none_or(Vec::is_empty)
    {
        result["handoff_deliveries"] = result["artifact"]["handoff_deliveries"].clone();
    }
    if !result["artifact"]["handoff_warnings"]
        .as_array()
        .is_none_or(Vec::is_empty)
    {
        result["handoff_warnings"] = result["artifact"]["handoff_warnings"].clone();
    }
    Ok(result)
}

#[derive(Debug, Clone, Default)]
struct ResearchHandoffOutcome {
    deliveries: Vec<ResearchHandoffDelivery>,
    warnings: Vec<String>,
}

fn deliver_research_handoff(
    task_id: &str,
    task: &Value,
    entry: &ResearchContextEntry,
) -> ResearchHandoffOutcome {
    let mut outcome = ResearchHandoffOutcome::default();
    let mut targets = Vec::new();
    if let Some(assignee) = string_field(task, "assignee")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        targets.push(("worker".to_string(), assignee.to_string()));
    }

    if let Some(supervisor) = resolve_supervisor_name(None) {
        targets.push(("supervisor".to_string(), supervisor));
    } else {
        let warning =
            "research handoff could not notify supervisor: no live supervisor session resolved"
                .to_string();
        outcome.deliveries.push(undelivered_research_handoff(
            "supervisor",
            "supervisor",
            warning.clone(),
        ));
        outcome.warnings.push(warning);
    }

    let existing_targets = existing_research_handoff_targets(task, &entry.artifact_id);
    let mut seen_targets = HashSet::new();
    let raw_message = build_research_handoff_message(task_id, task, entry);
    let options = load_context_tool_options();
    let message = compact_model_context_with_notice(
        &raw_message,
        &options.compression,
        ContextCompressionTarget::ResearchHandoff,
        "task research_context artifact files",
    );
    let from = caller_name();

    for (target_role, target) in targets {
        if !seen_targets.insert(target.clone()) {
            continue;
        }
        if existing_targets.contains(&target) {
            outcome.deliveries.push(ResearchHandoffDelivery {
                target,
                target_role,
                status: "skipped_duplicate".to_string(),
                method: None,
                prompt_id: None,
                warning: None,
                queued_at: Utc::now(),
            });
            continue;
        }

        let delivery = try_deliver_message(&target, &from, &message);
        let warning = (!delivery.queued).then(|| {
            format!(
                "research handoff could not notify {target_role} '{target}': {}",
                delivery.method
            )
        });
        if let Some(warning) = warning.as_ref() {
            outcome.warnings.push(warning.clone());
        }
        outcome.deliveries.push(ResearchHandoffDelivery {
            target,
            target_role,
            status: if delivery.queued { "queued" } else { "failed" }.to_string(),
            method: Some(delivery.method),
            prompt_id: (!delivery.prompt_id.is_empty()).then_some(delivery.prompt_id),
            warning,
            queued_at: Utc::now(),
        });
    }

    outcome
}

fn existing_research_handoff_targets(task: &Value, artifact_id: &str) -> HashSet<String> {
    task.get("research_context")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|entry| string_field(entry, "artifact_id") == Some(artifact_id))
        .filter_map(|entry| entry.get("handoff_deliveries").and_then(Value::as_array))
        .flatten()
        .filter_map(|delivery| string_field(delivery, "target"))
        .filter(|target| !target.trim().is_empty())
        .map(str::to_string)
        .collect()
}

fn undelivered_research_handoff(
    target: &str,
    target_role: &str,
    warning: String,
) -> ResearchHandoffDelivery {
    ResearchHandoffDelivery {
        target: target.to_string(),
        target_role: target_role.to_string(),
        status: "failed".to_string(),
        method: None,
        prompt_id: None,
        warning: Some(warning),
        queued_at: Utc::now(),
    }
}

fn build_research_handoff_message(
    task_id: &str,
    task: &Value,
    entry: &ResearchContextEntry,
) -> String {
    let task_title = string_field(task, "title").unwrap_or("Untitled task");
    let mut lines = vec![
        format!("Research artifact attached for task {task_id}: {task_title}"),
        format!("Artifact: {} / {}", entry.artifact_id, entry.title),
        format!("Summary: {}", compact_line(&entry.summary, 700)),
    ];
    if let Some(confidence) = entry.confidence.as_deref() {
        lines.push(format!("Confidence: {}", compact_line(confidence, 120)));
    }
    if !entry.citations.is_empty() {
        let first = entry.citations.first().map(String::as_str).unwrap_or("");
        let citation_summary = if first.is_empty() {
            format!("{} citation(s)", entry.citations.len())
        } else {
            format!(
                "{} citation(s); first: {}",
                entry.citations.len(),
                compact_line(first, 180)
            )
        };
        lines.push(format!("Citations: {citation_summary}"));
    }
    lines.push(format!("Brief: {}", entry.artifact_path));
    if !entry.structured_path.is_empty() {
        lines.push(format!("Data: {}", entry.structured_path));
    }
    lines.push(
        "Next: refresh task context before continuing; treat research as advisory and verify claims."
            .to_string(),
    );
    lines.join("\n")
}

fn attach_action(args: &Value) -> Result<Value, String> {
    let task_id = required_string_arg(args, "task_id")?;
    let artifact_id = required_string_arg(args, "artifact_id")?;
    let manifest = read_manifest(&task_id)?
        .ok_or_else(|| format!("research manifest not found for task '{task_id}'"))?;
    let entry = manifest
        .artifacts
        .iter()
        .find(|entry| entry.artifact_id == artifact_id)
        .ok_or_else(|| format!("research artifact '{artifact_id}' not found"))?;
    attach_artifact_to_task(&task_id, entry)?;
    Ok(serde_json::json!({ "status": "ok", "artifact_id": artifact_id }))
}

fn detach_action(args: &Value) -> Result<Value, String> {
    let task_id = required_string_arg(args, "task_id")?;
    let artifact_id = required_string_arg(args, "artifact_id")?;
    let mut task = read_task(&task_id).ok_or_else(|| format!("task '{task_id}' not found"))?;
    let before = task
        .get("research_context")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if let Some(context) = task
        .get_mut("research_context")
        .and_then(Value::as_array_mut)
    {
        context.retain(|entry| string_field(entry, "artifact_id") != Some(artifact_id.as_str()));
    }
    write_task(&task_id, &task)?;
    let after = task
        .get("research_context")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    Ok(serde_json::json!({
        "status": "ok",
        "removed": before.saturating_sub(after),
    }))
}

fn archive_action(args: &Value) -> Result<Value, String> {
    let task_id = required_string_arg(args, "task_id")?;
    let job_id = required_string_arg(args, "job_id")?;
    let mut job = read_job(&task_id, &job_id)?
        .ok_or_else(|| format!("research job '{job_id}' not found for task '{task_id}'"))?;
    job.status = JOB_STATUS_ARCHIVED.to_string();
    job.updated_at = Utc::now();
    write_job(&job)?;
    Ok(serde_json::json!({ "status": "ok", "job": job }))
}

fn queue_matching_routes(
    config: &BrehonConfig,
    task: &Value,
    trigger: ResearchTrigger,
    requested_by: &str,
    force: bool,
) -> Result<Vec<QueuedJob>, String> {
    let mut queued = Vec::new();
    let mut total_active = active_jobs_for_task(task_id_from_task(task)?).unwrap_or(0);
    for route in &config.research.routes {
        if !route.enabled || route.trigger != trigger {
            continue;
        }
        if !route_matches(task, &route.criteria) {
            continue;
        }
        if total_active >= config.research.defaults.max_parallel_jobs as usize {
            break;
        }
        let before = queued.len();
        let route_queued = queue_route(config, task, route, requested_by, force)?;
        total_active += route_queued.len();
        queued.extend(route_queued);
        if !route.continue_ && queued.len() > before {
            break;
        }
    }
    Ok(queued)
}

fn queue_route(
    config: &BrehonConfig,
    task: &Value,
    route: &ResearchRouteConfig,
    requested_by: &str,
    force: bool,
) -> Result<Vec<QueuedJob>, String> {
    let mut queued = Vec::new();
    let mut emitted = 0usize;
    for template in &route.jobs {
        if route
            .max_jobs_per_task
            .is_some_and(|max| emitted >= max as usize)
        {
            break;
        }
        if !force && route_job_already_exists(task_id_from_task(task)?, &route.id, &template.id)? {
            continue;
        }
        let Some(pool) = config
            .research
            .pools
            .iter()
            .find(|pool| pool.id == template.pool)
        else {
            continue;
        };
        let queued_job = queue_job(
            config,
            task,
            Some(route),
            template,
            pool,
            "route",
            requested_by,
            force,
        )?;
        emitted += 1;
        queued.push(queued_job);
    }
    Ok(queued)
}

fn queue_job(
    config: &BrehonConfig,
    task: &Value,
    route: Option<&ResearchRouteConfig>,
    template: &ResearchJobTemplateConfig,
    pool: &ResearchPoolConfig,
    origin: &str,
    requested_by: &str,
    force: bool,
) -> Result<QueuedJob, String> {
    let task_id = task_id_from_task(task)?.to_string();
    if !force && route.is_none() {
        let open_cost = requested_cost_for_task(&task_id, Some(&pool.role))?;
        if open_cost + pool.cost_units > config.research.worker_requests.max_cost_units_per_task {
            return Err(format!(
                "research request for task '{task_id}' would exceed max_cost_units_per_task"
            ));
        }
    }

    let prompt = render_prompt_template(task, template)?;
    let now = Utc::now();
    let job_id = next_job_id(&task_id, route.map(|route| route.id.as_str()), &template.id)?;
    let record = ResearchJobRecord {
        job_id,
        task_id,
        route_id: route.map(|route| route.id.clone()),
        template_id: template.id.clone(),
        pool: pool.id.clone(),
        lane: pool.lane.clone(),
        role: pool.role.clone(),
        status: JOB_STATUS_QUEUED.to_string(),
        origin: origin.to_string(),
        prompt,
        cost_units: pool.cost_units,
        requested_by: requested_by.to_string(),
        assigned_to: None,
        artifact_id: None,
        depends_on: template.depends_on.clone(),
        warnings: Vec::new(),
        created_at: now,
        updated_at: now,
    };
    write_job(&record)?;
    let notified_agents = notify_research_agents(&record);
    Ok(QueuedJob {
        record,
        notified_agents,
    })
}

fn notify_research_agents(job: &ResearchJobRecord) -> Vec<String> {
    let Some(root) = brehon_root() else {
        return Vec::new();
    };
    let sessions_dir = root.join("runtime").join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return Vec::new();
    };
    let from = caller_name();
    let mut notified = Vec::new();
    let message = format!(
        "Research job queued for task {}.\n\
         job_id: {}\n\
         pool: {}\n\
         role: {}\n\n\
         Claim it with: research action=claim_next pool={}\n\
         Then submit with: research action=submit task_id={} job_id={} summary=\"...\" content=\"...\" citations='[...]'",
        job.task_id, job.job_id, job.pool, job.role, job.pool, job.task_id, job.job_id
    );
    for entry in entries.flatten() {
        if entry.path().extension().is_none_or(|ext| ext != "json")
            || entry.file_name().to_string_lossy().starts_with('.')
        {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(session) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        if !session_is_live(&session) || !session_matches_current_runtime(&session) {
            continue;
        }
        if session.get("role").and_then(Value::as_str) != Some("research") {
            continue;
        }
        let agent_type = session
            .get("agent_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !agent_type.is_empty() && agent_type != job.lane && agent_type != job.pool {
            continue;
        }
        let Some(name) = session.get("name").and_then(Value::as_str) else {
            continue;
        };
        if try_deliver_message(name, &from, &message).queued {
            notified.push(name.to_string());
        }
    }
    notified
}

fn resolve_request_pool<'a>(
    config: &'a ResearchConfig,
    args: &Value,
) -> Result<&'a ResearchPoolConfig, String> {
    if let Some(pool_id) = string_arg(args, "pool") {
        return config
            .pools
            .iter()
            .find(|pool| pool.id == pool_id)
            .ok_or_else(|| format!("research pool '{pool_id}' is not configured"));
    }
    if let Some(role) = string_arg(args, "role") {
        return config
            .pools
            .iter()
            .find(|pool| pool.role == role)
            .ok_or_else(|| format!("no research pool is configured for role '{role}'"));
    }
    config
        .pools
        .first()
        .ok_or_else(|| "research has no pools configured".to_string())
}

fn enforce_worker_request_limits(
    config: &ResearchConfig,
    task_id: &str,
    pool: &ResearchPoolConfig,
) -> Result<(), String> {
    if caller_role() != "worker" {
        return Ok(());
    }
    if !config.worker_requests.enabled {
        return Err("research.worker_requests.enabled is false".to_string());
    }
    if !config.worker_requests.allowed_roles.is_empty()
        && !config
            .worker_requests
            .allowed_roles
            .iter()
            .any(|role| role == &pool.role)
    {
        return Err(format!(
            "research role '{}' is not listed in worker_requests.allowed_roles",
            pool.role
        ));
    }
    let existing = requested_jobs_for_task(task_id)?;
    if existing.len() >= config.worker_requests.max_requests_per_task as usize {
        return Err(format!(
            "task '{task_id}' already reached max_requests_per_task={}",
            config.worker_requests.max_requests_per_task
        ));
    }
    if pool.cost_units > config.worker_requests.max_cost_units_per_request {
        return Err(format!(
            "research pool '{}' costs {} units, above max_cost_units_per_request={}",
            pool.id, pool.cost_units, config.worker_requests.max_cost_units_per_request
        ));
    }
    let current_cost = existing.iter().map(|job| job.cost_units).sum::<u32>();
    if current_cost + pool.cost_units > config.worker_requests.max_cost_units_per_task {
        return Err(format!(
            "task '{task_id}' would exceed max_cost_units_per_task={}",
            config.worker_requests.max_cost_units_per_task
        ));
    }
    Ok(())
}

fn requested_jobs_for_task(task_id: &str) -> Result<Vec<ResearchJobRecord>, String> {
    Ok(read_jobs_for_task(task_id)?
        .into_iter()
        .filter(|job| job.origin == "worker_request")
        .collect())
}

fn requested_cost_for_task(task_id: &str, role: Option<&str>) -> Result<u32, String> {
    Ok(requested_jobs_for_task(task_id)?
        .into_iter()
        .filter(|job| role.is_none_or(|role| job.role == role))
        .map(|job| job.cost_units)
        .sum())
}

fn active_jobs_for_task(task_id: &str) -> Result<usize, String> {
    Ok(read_jobs_for_task(task_id)?
        .into_iter()
        .filter(|job| matches!(job.status.as_str(), JOB_STATUS_QUEUED | JOB_STATUS_RUNNING))
        .count())
}

fn running_jobs(pool: Option<&str>) -> Result<usize, String> {
    Ok(read_all_jobs()?
        .into_iter()
        .filter(|job| job.status == JOB_STATUS_RUNNING && pool.is_none_or(|pool| job.pool == pool))
        .count())
}

fn route_job_already_exists(
    task_id: &str,
    route_id: &str,
    template_id: &str,
) -> Result<bool, String> {
    Ok(read_jobs_for_task(task_id)?.into_iter().any(|job| {
        job.route_id.as_deref() == Some(route_id)
            && job.template_id == template_id
            && job.status != JOB_STATUS_FAILED
            && job.status != JOB_STATUS_ARCHIVED
    }))
}

fn dependencies_satisfied(job: &ResearchJobRecord) -> Result<bool, String> {
    if job.depends_on.is_empty() {
        return Ok(true);
    }
    let completed: HashSet<String> = read_jobs_for_task(&job.task_id)?
        .into_iter()
        .filter(|candidate| {
            candidate.route_id == job.route_id && candidate.status == JOB_STATUS_COMPLETED
        })
        .map(|candidate| candidate.template_id)
        .collect();
    Ok(job
        .depends_on
        .iter()
        .all(|dependency| completed.contains(dependency)))
}

fn route_matches(task: &Value, criteria: &ResearchRouteMatchConfig) -> bool {
    if criteria.default {
        return true;
    }

    let mut checks = Vec::new();
    if let Some(task_type) = criteria.task_type.as_deref() {
        checks.push(field_equals(task, "task_type", task_type));
    }
    if let Some(priority) = criteria.priority.as_deref() {
        checks.push(field_equals(task, "priority", priority));
    }
    if let Some(work_class) = criteria.work_class.as_deref() {
        checks.push(task_contains_any(
            task,
            &["work_class", "work_classes"],
            &[work_class],
        ));
    }
    if !criteria.work_classes.is_empty() {
        checks.push(task_contains_any(
            task,
            &["work_class", "work_classes"],
            &criteria
                .work_classes
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        ));
    }
    if !criteria.title_any.is_empty() {
        checks.push(text_contains_any(
            &string_field(task, "title")
                .unwrap_or_default()
                .to_ascii_lowercase(),
            &criteria.title_any,
        ));
    }
    if !criteria.text_any.is_empty() {
        checks.push(text_contains_any(
            &task_search_text(task),
            &criteria.text_any,
        ));
    }
    if !criteria.task_status_any.is_empty() {
        checks.push(task_contains_any(
            task,
            &["status"],
            &criteria
                .task_status_any
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        ));
    }
    if !criteria.task_size_any.is_empty() {
        checks.push(task_contains_any(
            task,
            &["task_size", "size"],
            &criteria
                .task_size_any
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        ));
    }
    if !criteria.changed_paths_any.is_empty() {
        checks.push(task_contains_any(
            task,
            &["changed_paths", "changed_files", "file_hints"],
            &criteria
                .changed_paths_any
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        ));
    }
    if !criteria.changed_paths_all.is_empty() {
        checks.push(criteria.changed_paths_all.iter().all(|needle| {
            task_contains_any(
                task,
                &["changed_paths", "changed_files", "file_hints"],
                &[needle],
            )
        }));
    }
    if !criteria.source_plan_any.is_empty() {
        checks.push(task_contains_any(
            task,
            &[
                "source_plan",
                "plan_steps",
                "implementation_notes",
                "description",
            ],
            &criteria
                .source_plan_any
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        ));
    }

    !checks.is_empty() && checks.into_iter().all(|matched| matched)
}

fn field_equals(task: &Value, key: &str, expected: &str) -> bool {
    string_field(task, key)
        .map(|value| value.eq_ignore_ascii_case(expected))
        .unwrap_or(false)
}

fn task_contains_any(task: &Value, keys: &[&str], needles: &[&str]) -> bool {
    let needles = needles
        .iter()
        .map(|needle| needle.to_ascii_lowercase())
        .collect::<Vec<_>>();
    keys.iter().any(|key| {
        values_for_key(task, key).iter().any(|value| {
            let value = value.to_ascii_lowercase();
            needles
                .iter()
                .any(|needle| value == *needle || value.contains(needle))
        })
    })
}

fn text_contains_any(haystack: &str, needles: &[String]) -> bool {
    let haystack = haystack.to_ascii_lowercase();
    needles
        .iter()
        .any(|needle| haystack.contains(&needle.to_ascii_lowercase()))
}

fn task_search_text(task: &Value) -> String {
    let mut parts = Vec::new();
    for key in [
        "title",
        "description",
        "notes",
        "implementation_notes",
        "blockers",
    ] {
        if let Some(value) = string_field(task, key) {
            parts.push(value.to_string());
        }
    }
    for key in [
        "acceptance_criteria",
        "file_hints",
        "constraints",
        "test_requirements",
        "plan_steps",
    ] {
        parts.extend(values_for_key(task, key));
    }
    parts.join("\n").to_ascii_lowercase()
}

fn render_prompt_template(
    task: &Value,
    template: &ResearchJobTemplateConfig,
) -> Result<String, String> {
    let mut prompt = template.prompt_template.clone();
    let replacements = [
        ("task_id", task_id_from_task(task)?.to_string()),
        (
            "title",
            string_field(task, "title").unwrap_or_default().to_string(),
        ),
        (
            "description",
            string_field(task, "description")
                .unwrap_or_default()
                .to_string(),
        ),
        (
            "priority",
            string_field(task, "priority")
                .unwrap_or_default()
                .to_string(),
        ),
        (
            "task_type",
            string_field(task, "task_type")
                .unwrap_or("task")
                .to_string(),
        ),
        ("file_hints", values_for_key(task, "file_hints").join("\n")),
        (
            "acceptance_criteria",
            values_for_key(task, "acceptance_criteria").join("\n"),
        ),
        ("plan_steps", values_for_key(task, "plan_steps").join("\n")),
        (
            "prior_research_manifest",
            task_id_from_task(task)
                .ok()
                .and_then(|task_id| read_manifest(task_id).ok().flatten())
                .map(|manifest| {
                    manifest
                        .artifacts
                        .iter()
                        .map(|entry| {
                            format!(
                                "- {} [{}] {}: {}",
                                entry.artifact_id, entry.role, entry.title, entry.summary
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
        ),
    ];
    for (key, value) in replacements {
        prompt = prompt.replace(&format!("{{{{{key}}}}}"), &value);
    }
    Ok(prompt)
}

fn parse_trigger(raw: &str) -> Result<ResearchTrigger, String> {
    match raw.trim() {
        "before_assignment" => Ok(ResearchTrigger::BeforeAssignment),
        "before_review" => Ok(ResearchTrigger::BeforeReview),
        "manual" => Ok(ResearchTrigger::Manual),
        other => Err(format!("unknown research trigger '{other}'")),
    }
}

fn structured_arg(args: &Value) -> Option<StructuredResearchArtifact> {
    let value = args.get("structured")?.clone();
    serde_json::from_value(value).ok()
}

fn render_brief(artifact: &StructuredResearchArtifact) -> String {
    let mut out = format!("# {}\n\n{}\n", artifact.title, artifact.summary);
    if !artifact.findings.is_empty() {
        out.push_str("\n## Findings\n");
        for finding in &artifact.findings {
            out.push_str(&format!("- {finding}\n"));
        }
    }
    if !artifact.citations.is_empty() {
        out.push_str("\n## Citations\n");
        for citation in &artifact.citations {
            out.push_str(&format!("- {citation}\n"));
        }
    }
    if let Some(confidence) = &artifact.confidence {
        out.push_str(&format!("\nConfidence: {confidence}\n"));
    }
    out
}

fn append_manifest_entry(task_id: &str, entry: ResearchContextEntry) -> Result<(), String> {
    let mut manifest = read_manifest(task_id)?.unwrap_or_else(|| ResearchManifest {
        task_id: task_id.to_string(),
        updated_at: Utc::now(),
        artifacts: Vec::new(),
    });
    manifest
        .artifacts
        .retain(|existing| existing.artifact_id != entry.artifact_id);
    manifest.artifacts.push(entry);
    manifest.updated_at = Utc::now();
    write_manifest(&manifest)?;
    write_global_manifest()?;
    Ok(())
}

fn write_manifest(manifest: &ResearchManifest) -> Result<(), String> {
    let path = task_research_dir(&manifest.task_id)?.join("manifest.yaml");
    let yaml = serde_yaml::to_string(manifest)
        .map_err(|err| format!("failed to serialize research manifest: {err}"))?;
    atomic_write(&path, yaml.as_bytes())
}

fn read_manifest(task_id: &str) -> Result<Option<ResearchManifest>, String> {
    let path = task_research_dir(task_id)?.join("manifest.yaml");
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|err| format!("failed to read research manifest {}: {err}", path.display()))?;
    serde_yaml::from_str(&content).map(Some).map_err(|err| {
        format!(
            "failed to parse research manifest {}: {err}",
            path.display()
        )
    })
}

fn read_all_manifests() -> Result<Vec<ResearchManifest>, String> {
    let mut manifests = Vec::new();
    let root = research_root()?;
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Ok(manifests);
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let path = entry.path().join("manifest.yaml");
        if !path.exists() {
            continue;
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|err| format!("failed to read research manifest {}: {err}", path.display()))?;
        if let Ok(manifest) = serde_yaml::from_str::<ResearchManifest>(&content) {
            manifests.push(manifest);
        }
    }
    manifests.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
    Ok(manifests)
}

fn write_global_manifest() -> Result<(), String> {
    let manifests = read_all_manifests()?;
    let global = ResearchGlobalManifest {
        updated_at: Utc::now(),
        tasks: manifests
            .iter()
            .map(|manifest| ResearchTaskManifestSummary {
                task_id: manifest.task_id.clone(),
                artifact_count: manifest.artifacts.len(),
                updated_at: manifest.updated_at,
            })
            .collect(),
    };
    let path = research_root()?.join("manifest.yaml");
    let yaml = serde_yaml::to_string(&global)
        .map_err(|err| format!("failed to serialize global research manifest: {err}"))?;
    atomic_write(&path, yaml.as_bytes())
}

fn attach_artifact_to_task(task_id: &str, entry: &ResearchContextEntry) -> Result<(), String> {
    let mut task = read_task(task_id).ok_or_else(|| format!("task '{task_id}' not found"))?;
    let serialized = serde_json::to_value(entry)
        .map_err(|err| format!("failed to serialize research context entry: {err}"))?;
    let context = task
        .as_object_mut()
        .ok_or_else(|| format!("task '{task_id}' is not a JSON object"))?
        .entry("research_context")
        .or_insert_with(|| Value::Array(Vec::new()));
    let context = context
        .as_array_mut()
        .ok_or_else(|| "task.research_context exists but is not an array".to_string())?;
    context.retain(|existing| {
        string_field(existing, "artifact_id") != Some(entry.artifact_id.as_str())
    });
    context.push(serialized);
    if context.len() > 20 {
        let remove = context.len() - 20;
        context.drain(0..remove);
    }
    write_task(task_id, &task)
}

fn read_all_jobs() -> Result<Vec<ResearchJobRecord>, String> {
    let mut jobs = Vec::new();
    let root = research_root()?;
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Ok(jobs);
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let jobs_dir = entry.path().join("jobs");
        let Ok(job_entries) = std::fs::read_dir(jobs_dir) else {
            continue;
        };
        for job_entry in job_entries.flatten() {
            if job_entry.path().extension().and_then(|ext| ext.to_str()) != Some("json")
                || job_entry.file_name().to_string_lossy().starts_with('.')
            {
                continue;
            }
            let content = std::fs::read_to_string(job_entry.path()).map_err(|err| {
                format!(
                    "failed to read research job {}: {err}",
                    job_entry.path().display()
                )
            })?;
            let job = serde_json::from_str::<ResearchJobRecord>(&content).map_err(|err| {
                format!(
                    "failed to parse research job {}: {err}",
                    job_entry.path().display()
                )
            })?;
            jobs.push(job);
        }
    }
    Ok(jobs)
}

fn read_jobs_for_task(task_id: &str) -> Result<Vec<ResearchJobRecord>, String> {
    let dir = jobs_dir(task_id)?;
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut jobs = Vec::new();
    for entry in entries.flatten() {
        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("json")
            || entry.file_name().to_string_lossy().starts_with('.')
        {
            continue;
        }
        let content = std::fs::read_to_string(entry.path()).map_err(|err| {
            format!(
                "failed to read research job {}: {err}",
                entry.path().display()
            )
        })?;
        jobs.push(
            serde_json::from_str::<ResearchJobRecord>(&content).map_err(|err| {
                format!(
                    "failed to parse research job {}: {err}",
                    entry.path().display()
                )
            })?,
        );
    }
    Ok(jobs)
}

fn read_job(task_id: &str, job_id: &str) -> Result<Option<ResearchJobRecord>, String> {
    let path = job_path(task_id, job_id)?;
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|err| format!("failed to read research job {}: {err}", path.display()))?;
    serde_json::from_str(&content)
        .map(Some)
        .map_err(|err| format!("failed to parse research job {}: {err}", path.display()))
}

fn write_job(job: &ResearchJobRecord) -> Result<(), String> {
    let path = job_path(&job.task_id, &job.job_id)?;
    let payload = serde_json::to_vec_pretty(job)
        .map_err(|err| format!("failed to serialize research job: {err}"))?;
    atomic_write(&path, &payload)
}

fn next_job_id(task_id: &str, route_id: Option<&str>, template_id: &str) -> Result<String, String> {
    let prefix = match route_id {
        Some(route_id) => format!(
            "RJOB-{}-{}-{}",
            sanitize_id(task_id),
            sanitize_id(route_id),
            sanitize_id(template_id)
        ),
        None => format!("RJOB-{}-{}", sanitize_id(task_id), sanitize_id(template_id)),
    };
    let seq = next_sequence(&jobs_dir(task_id)?, &prefix)?;
    Ok(format!("{prefix}-{seq:03}"))
}

fn next_artifact_id(task_id: &str, template_id: &str) -> Result<String, String> {
    let prefix = format!("RCH-{}-{}", sanitize_id(task_id), sanitize_id(template_id));
    let seq = next_sequence(&task_research_dir(task_id)?, &prefix)?;
    Ok(format!("{prefix}-{seq:03}"))
}

fn next_sequence(dir: &Path, prefix: &str) -> Result<u32, String> {
    std::fs::create_dir_all(dir)
        .map_err(|err| format!("failed to create research dir {}: {err}", dir.display()))?;
    let mut max_seq = 0u32;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(1);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(prefix) {
            continue;
        }
        let name = name.strip_suffix(".json").unwrap_or(&name);
        if let Some(raw) = name.rsplit('-').next() {
            if let Ok(seq) = raw.parse::<u32>() {
                max_seq = max_seq.max(seq);
            }
        }
    }
    Ok(max_seq + 1)
}

fn job_path(task_id: &str, job_id: &str) -> Result<PathBuf, String> {
    Ok(jobs_dir(task_id)?.join(format!("{}.json", sanitize_id(job_id))))
}

fn jobs_dir(task_id: &str) -> Result<PathBuf, String> {
    Ok(task_research_dir(task_id)?.join("jobs"))
}

fn artifact_path(task_id: &str, artifact_id: &str, name: &str) -> Result<PathBuf, String> {
    Ok(artifact_dir(task_id, artifact_id)?.join(name))
}

fn artifact_dir(task_id: &str, artifact_id: &str) -> Result<PathBuf, String> {
    Ok(task_research_dir(task_id)?.join(sanitize_id(artifact_id)))
}

fn task_research_dir(task_id: &str) -> Result<PathBuf, String> {
    Ok(research_root()?.join(sanitize_id(task_id)))
}

fn research_root() -> Result<PathBuf, String> {
    let config = load_project_config()
        .map(|config| config.research)
        .unwrap_or_default();
    research_root_from_config(&config)
}

fn research_root_from_config(config: &ResearchConfig) -> Result<PathBuf, String> {
    let project_root = project_root().ok_or_else(|| "No BREHON_ROOT available".to_string())?;
    let path = PathBuf::from(&config.artifact_root);
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(project_root.join(path))
}

fn brehon_root() -> Option<PathBuf> {
    std::env::var("BREHON_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(".brehon")))
}

fn project_root() -> Option<PathBuf> {
    let root = brehon_root()?;
    if root.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
        return root.parent().map(PathBuf::from);
    }
    Some(root)
}

fn load_project_config() -> Result<BrehonConfig, String> {
    let root = project_root().ok_or_else(|| "No BREHON_ROOT available".to_string())?;
    brehon_config::load_config(Some(&root)).map_err(|err| {
        format!(
            "failed to load project config from {}: {err}",
            root.display()
        )
    })
}

fn tasks_dir() -> Option<PathBuf> {
    Some(brehon_root()?.join("runtime").join("tasks"))
}

fn read_task(task_id: &str) -> Option<Value> {
    let path = tasks_dir()?.join(format!("{}.json", sanitize_id(task_id)));
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_task(task_id: &str, task: &Value) -> Result<(), String> {
    let path = tasks_dir()
        .ok_or_else(|| "No BREHON_ROOT available".to_string())?
        .join(format!("{}.json", sanitize_id(task_id)));
    let payload = serde_json::to_vec_pretty(task)
        .map_err(|err| format!("failed to serialize task '{task_id}': {err}"))?;
    atomic_write(&path, &payload)
}

fn atomic_write(path: &Path, payload: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create dir {}: {err}", parent.display()))?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp-{}",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("tmp"),
        std::process::id()
    ));
    std::fs::write(&tmp, payload)
        .map_err(|err| format!("failed to write temp file {}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|err| format!("failed to install {}: {err}", path.display()))
}

fn project_relative_path(path: &Path) -> String {
    project_root()
        .and_then(|root| path.strip_prefix(root).ok().map(Path::to_path_buf))
        .unwrap_or_else(|| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn caller_name() -> String {
    std::env::var("BREHON_AGENT_NAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "user".to_string())
}

fn caller_role() -> String {
    std::env::var("BREHON_AGENT_ROLE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "human".to_string())
}

fn task_id_from_task(task: &Value) -> Result<&str, String> {
    string_field(task, "task_id")
        .or_else(|| string_field(task, "id"))
        .ok_or_else(|| "task is missing task_id".to_string())
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn values_for_key(value: &Value, key: &str) -> Vec<String> {
    match value.get(key) {
        Some(Value::String(raw)) if !raw.trim().is_empty() => vec![raw.trim().to_string()],
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn required_string_arg(args: &Value, key: &str) -> Result<String, String> {
    string_arg(args, key).ok_or_else(|| format!("research action requires non-empty {key}"))
}

fn string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn string_array_arg(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn bool_arg(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn sanitize_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn compact_line(value: &str, max_chars: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    let mut out = compact
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::ContentBlock;
    use crate::tools::agent::prompt_queue_root;
    use crate::tools::{Tool, TEST_ENV_LOCK};
    use brehon_mux::{PromptQueueEntry, SessionScopedQueue};

    struct EnvGuard {
        previous: BTreeMap<String, Option<String>>,
    }

    impl EnvGuard {
        fn set(values: &[(&str, &str)]) -> Self {
            let previous = values
                .iter()
                .map(|(key, _)| ((*key).to_string(), std::env::var(key).ok()))
                .collect();
            for (key, value) in values {
                std::env::set_var(key, value);
            }
            Self { previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.previous {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }

    fn text_json(result: ToolResult) -> Value {
        match &result.content[0] {
            ContentBlock::Text { text } => serde_json::from_str(text).expect("json"),
            _ => panic!("expected text content"),
        }
    }

    fn write_task_fixture(brehon_root: &Path, task_id: &str) {
        write_task_fixture_with_assignee(brehon_root, task_id, None);
    }

    fn write_task_fixture_with_assignee(brehon_root: &Path, task_id: &str, assignee: Option<&str>) {
        let tasks = brehon_root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks).expect("tasks dir");
        let mut task = serde_json::json!({
            "task_id": task_id,
            "title": "Implement PFCP association state",
            "description": "Use TS 29.244 and RFC context.",
            "status": "pending",
            "priority": "high",
            "task_type": "task",
            "file_hints": ["crates/pfcp/src/lib.rs"],
            "plan_steps": ["map normative requirements"]
        });
        if let Some(assignee) = assignee {
            task["assignee"] = Value::String(assignee.to_string());
            task["status"] = Value::String("in_progress".to_string());
        }
        std::fs::write(
            tasks.join(format!("{task_id}.json")),
            serde_json::to_string_pretty(&task).unwrap(),
        )
        .expect("task");
    }

    fn write_config_fixture(project_root: &Path) {
        let config_dir = project_root.join(".brehon");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(
            config_dir.join("config.yaml"),
            r#"
version: 1
lanes:
  research-cheap:
    launcher: claude
research:
  enabled: true
  pools:
    - id: specs
      lane: research-cheap
      instruction_profile: spec brief
      role: normative_requirements
      min: 0
      max: 2
      cost_units: 1
  routes:
    - id: pfcp-specs
      trigger: before_assignment
      match:
        text_any: ["PFCP"]
      jobs:
        - id: normative-requirements
          pool: specs
          prompt_template: "Research {{task_id}}: {{title}}\n{{file_hints}}"
"#,
        )
        .expect("config");
    }

    fn write_research_job_fixture(
        task_id: &str,
        job_id: &str,
        pool: &str,
        status: &str,
    ) -> ResearchJobRecord {
        let now = Utc::now();
        let job = ResearchJobRecord {
            job_id: job_id.to_string(),
            task_id: task_id.to_string(),
            route_id: None,
            template_id: "normative-requirements".to_string(),
            pool: pool.to_string(),
            lane: "research-cheap".to_string(),
            role: "normative_requirements".to_string(),
            status: status.to_string(),
            origin: "test".to_string(),
            prompt: "Research the task.".to_string(),
            cost_units: 1,
            requested_by: "test".to_string(),
            assigned_to: (status == JOB_STATUS_RUNNING).then(|| "research-running".to_string()),
            artifact_id: None,
            depends_on: Vec::new(),
            warnings: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        write_job(&job).expect("write research job");
        job
    }

    fn drain_prompt_queue(brehon_root: &Path, session_name: &str) -> Vec<PromptQueueEntry> {
        let prompt_queue = SessionScopedQueue::<PromptQueueEntry>::new(
            session_name,
            prompt_queue_root(brehon_root),
        );
        prompt_queue
            .drain()
            .map(|entry| entry.expect("prompt entry should decode").entry)
            .collect()
    }

    #[tokio::test]
    async fn request_claim_and_submit_attaches_research_context_without_blocking_task() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let config_home = temp.path().join("xdg-config");
        std::fs::create_dir_all(&brehon_root).expect("brehon root");
        std::fs::create_dir_all(&config_home).expect("config home");
        write_config_fixture(temp.path());
        write_task_fixture(&brehon_root, "T-5g");
        let config_home = config_home.to_string_lossy().to_string();
        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", brehon_root.to_str().unwrap()),
            ("XDG_CONFIG_HOME", &config_home),
            ("BREHON_AGENT_NAME", "worker-1"),
            ("BREHON_AGENT_ROLE", "worker"),
        ]);

        let tool = ResearchTool::new();
        let request = text_json(
            tool.execute(serde_json::json!({
                "action": "request",
                "task_id": "T-5g",
                "role": "normative_requirements",
                "prompt": "Find the normative PFCP requirements."
            }))
            .await
            .unwrap(),
        );
        assert_eq!(request["status"], "queued");
        let job_id = request["job"]["job_id"].as_str().unwrap().to_string();

        std::env::set_var("BREHON_AGENT_NAME", "research-1");
        std::env::set_var("BREHON_AGENT_ROLE", "research");
        let claim = text_json(
            tool.execute(serde_json::json!({
                "action": "claim_next",
                "pool": "specs"
            }))
            .await
            .unwrap(),
        );
        assert_eq!(claim["status"], "claimed");
        assert_eq!(claim["job"]["job_id"], job_id);

        let submit = text_json(
            tool.execute(serde_json::json!({
                "action": "submit",
                "task_id": "T-5g",
                "job_id": job_id,
                "title": "PFCP normative map",
                "summary": "PFCP association state needs heartbeat and recovery timestamp handling.",
                "content": "## Findings\n- Track recovery timestamp.\n",
                "citations": ["3GPP TS 29.244"]
            }))
            .await
            .unwrap(),
        );
        assert_eq!(submit["status"], "ok");

        let task: Value = serde_json::from_str(
            &std::fs::read_to_string(brehon_root.join("runtime/tasks/T-5g.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(task["status"], "pending");
        assert_eq!(task["research_context"][0]["title"], "PFCP normative map");
        assert!(task["research_context"][0]["artifact_path"]
            .as_str()
            .unwrap()
            .ends_with("brief.md"));
    }

    #[tokio::test]
    async fn submit_delivers_compact_handoff_to_worker_and_supervisor() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let config_home = temp.path().join("xdg-config");
        std::fs::create_dir_all(&brehon_root).expect("brehon root");
        std::fs::create_dir_all(&config_home).expect("config home");
        write_config_fixture(temp.path());
        write_task_fixture_with_assignee(&brehon_root, "T-handoff", Some("worker-1"));
        let config_home = config_home.to_string_lossy().to_string();
        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", brehon_root.to_str().unwrap()),
            ("XDG_CONFIG_HOME", &config_home),
            ("BREHON_AGENT_NAME", "research-1"),
            ("BREHON_AGENT_ROLE", "research"),
            ("BREHON_SUPERVISOR_NAME", "supervisor-1"),
            ("BREHON_SESSION_NAME", "research-handoff-test"),
        ]);
        write_research_job_fixture(
            "T-handoff",
            "RJOB-T-handoff-spec-001",
            "specs",
            JOB_STATUS_RUNNING,
        );

        let submit = text_json(
            ResearchTool::new()
                .execute(serde_json::json!({
                    "action": "submit",
                    "task_id": "T-handoff",
                    "job_id": "RJOB-T-handoff-spec-001",
                    "title": "PFCP normative map",
                    "summary": "PFCP association state needs heartbeat and recovery timestamp handling.",
                    "confidence": "high",
                    "content": "## Full brief\nsecret full brief line that should not be pushed into handoff messages\n",
                    "citations": ["3GPP TS 29.244 section 5"]
                }))
                .await
                .unwrap(),
        );

        assert_eq!(submit["status"], "ok");
        assert!(submit["handoff_warnings"].is_null());
        let artifact_id = submit["artifact"]["artifact_id"].as_str().unwrap();
        let deliveries = submit["handoff_deliveries"].as_array().unwrap();
        assert_eq!(deliveries.len(), 2);
        assert!(deliveries
            .iter()
            .any(|delivery| delivery["target"] == "worker-1" && delivery["status"] == "queued"));
        assert!(
            deliveries
                .iter()
                .any(|delivery| delivery["target"] == "supervisor-1"
                    && delivery["status"] == "queued")
        );

        let prompts = drain_prompt_queue(&brehon_root, "research-handoff-test");
        assert_eq!(prompts.len(), 2);
        let worker_prompt = prompts
            .iter()
            .find(|prompt| prompt.target == "worker-1")
            .expect("worker prompt");
        assert_eq!(worker_prompt.from.as_deref(), Some("research-1"));
        assert!(worker_prompt.message.contains(artifact_id));
        assert!(worker_prompt.message.contains("PFCP normative map"));
        assert!(worker_prompt
            .message
            .contains("Summary: PFCP association state"));
        assert!(worker_prompt
            .message
            .contains("Brief: .brehon/runtime/research"));
        assert!(worker_prompt
            .message
            .contains("Data: .brehon/runtime/research"));
        assert!(worker_prompt.message.contains("Citations: 1 citation(s)"));
        assert!(!worker_prompt.message.contains("secret full brief line"));
        assert!(prompts.iter().any(|prompt| prompt.target == "supervisor-1"));

        let task: Value = serde_json::from_str(
            &std::fs::read_to_string(brehon_root.join("runtime/tasks/T-handoff.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(task["research_context"][0]["artifact_id"], artifact_id);
        assert_eq!(
            task["research_context"][0]["handoff_deliveries"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn submit_records_handoff_warning_when_prompt_queue_write_fails() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let config_home = temp.path().join("xdg-config");
        std::fs::create_dir_all(&brehon_root).expect("brehon root");
        std::fs::create_dir_all(&config_home).expect("config home");
        write_config_fixture(temp.path());
        write_task_fixture_with_assignee(&brehon_root, "T-warning", Some("worker-1"));
        std::fs::write(brehon_root.join("runtime/prompt-queue"), "not a directory")
            .expect("prompt queue blocker");
        let config_home = config_home.to_string_lossy().to_string();
        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", brehon_root.to_str().unwrap()),
            ("XDG_CONFIG_HOME", &config_home),
            ("BREHON_AGENT_NAME", "research-1"),
            ("BREHON_AGENT_ROLE", "research"),
            ("BREHON_SUPERVISOR_NAME", "supervisor-1"),
            ("BREHON_SESSION_NAME", "research-handoff-warning-test"),
        ]);
        write_research_job_fixture(
            "T-warning",
            "RJOB-T-warning-spec-001",
            "specs",
            JOB_STATUS_RUNNING,
        );

        let submit = text_json(
            ResearchTool::new()
                .execute(serde_json::json!({
                    "action": "submit",
                    "task_id": "T-warning",
                    "job_id": "RJOB-T-warning-spec-001",
                    "title": "PFCP warning map",
                    "summary": "Handoff warning path.",
                    "content": "## Findings\nPersist even if delivery fails.\n",
                    "citations": ["local fixture"]
                }))
                .await
                .unwrap(),
        );

        assert_eq!(submit["status"], "ok");
        assert!(submit["handoff_warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap()
                .contains("could not notify worker")));
        assert!(submit["handoff_deliveries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|delivery| delivery["target"] == "worker-1" && delivery["status"] == "failed"));
        assert!(brehon_root
            .join("runtime/research/T-warning")
            .join(submit["artifact"]["artifact_id"].as_str().unwrap())
            .join("brief.md")
            .exists());
        let task: Value = serde_json::from_str(
            &std::fs::read_to_string(brehon_root.join("runtime/tasks/T-warning.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(task["research_context"][0]["title"], "PFCP warning map");
        assert!(task["research_context"][0]["handoff_warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap()
                .contains("could not notify worker")));
    }

    #[test]
    fn handoff_delivery_skips_duplicate_artifact_targets() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        std::fs::create_dir_all(&brehon_root).expect("brehon root");
        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", brehon_root.to_str().unwrap()),
            ("BREHON_AGENT_NAME", "research-1"),
            ("BREHON_SUPERVISOR_NAME", "supervisor-1"),
            ("BREHON_SESSION_NAME", "research-handoff-dup-test"),
        ]);
        let artifact_id = "RCH-T-dup-spec-001";
        let task = serde_json::json!({
            "task_id": "T-dup",
            "title": "Duplicate handoff",
            "assignee": "worker-1",
            "research_context": [{
                "artifact_id": artifact_id,
                "handoff_deliveries": [{
                    "target": "worker-1",
                    "target_role": "worker",
                    "status": "queued",
                    "queued_at": Utc::now()
                }]
            }]
        });
        let entry = ResearchContextEntry {
            artifact_id: artifact_id.to_string(),
            job_id: "RJOB-T-dup-spec-001".to_string(),
            pool: "specs".to_string(),
            role: "normative_requirements".to_string(),
            title: "Duplicate map".to_string(),
            summary: "Do not deliver the same artifact to the same target twice.".to_string(),
            confidence: None,
            artifact_path: ".brehon/runtime/research/T-dup/RCH-T-dup-spec-001/brief.md".to_string(),
            structured_path: ".brehon/runtime/research/T-dup/RCH-T-dup-spec-001/artifact.yaml"
                .to_string(),
            citations: Vec::new(),
            supersedes: Vec::new(),
            handoff_deliveries: Vec::new(),
            handoff_warnings: Vec::new(),
            created_at: Utc::now(),
        };

        let outcome = deliver_research_handoff("T-dup", &task, &entry);

        assert!(outcome.deliveries.iter().any(|delivery| {
            delivery.target == "worker-1" && delivery.status == "skipped_duplicate"
        }));
        assert!(outcome
            .deliveries
            .iter()
            .any(|delivery| { delivery.target == "supervisor-1" && delivery.status == "queued" }));
        let prompts = drain_prompt_queue(&brehon_root, "research-handoff-dup-test");
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].target, "supervisor-1");
    }

    #[tokio::test]
    async fn research_role_cannot_queue_or_mutate_research_jobs() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let config_home = temp.path().join("xdg-config");
        std::fs::create_dir_all(&brehon_root).expect("brehon root");
        std::fs::create_dir_all(&config_home).expect("config home");
        write_config_fixture(temp.path());
        write_task_fixture(&brehon_root, "T-readonly");
        let config_home = config_home.to_string_lossy().to_string();
        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", brehon_root.to_str().unwrap()),
            ("XDG_CONFIG_HOME", &config_home),
            ("BREHON_AGENT_NAME", "research-1"),
            ("BREHON_AGENT_ROLE", "research"),
        ]);

        let result = ResearchTool::new()
            .execute(serde_json::json!({
                "action": "request",
                "task_id": "T-readonly",
                "role": "normative_requirements",
                "prompt": "Queueing jobs is not part of the research role."
            }))
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(true));
        if let ContentBlock::Text { text } = &result.content[0] {
            assert!(text.contains("Research agents may only use research actions"));
            assert!(text.contains("request"));
        }
    }

    #[tokio::test]
    async fn claim_next_respects_pool_max() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let config_home = temp.path().join("xdg-config");
        std::fs::create_dir_all(&brehon_root).expect("brehon root");
        std::fs::create_dir_all(&config_home).expect("config home");
        write_config_fixture(temp.path());
        let config_home = config_home.to_string_lossy().to_string();
        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", brehon_root.to_str().unwrap()),
            ("XDG_CONFIG_HOME", &config_home),
            ("BREHON_AGENT_NAME", "research-1"),
            ("BREHON_AGENT_ROLE", "research"),
        ]);
        write_research_job_fixture("T-pool", "running-1", "specs", JOB_STATUS_RUNNING);
        write_research_job_fixture("T-pool", "running-2", "specs", JOB_STATUS_RUNNING);
        write_research_job_fixture("T-pool", "queued-1", "specs", JOB_STATUS_QUEUED);

        let claim = text_json(
            ResearchTool::new()
                .execute(serde_json::json!({
                    "action": "claim_next",
                    "pool": "specs"
                }))
                .await
                .unwrap(),
        );

        assert_eq!(claim["status"], "idle");
        assert!(claim["message"]
            .as_str()
            .unwrap()
            .contains("available pool capacity"));
        let queued = read_job("T-pool", "queued-1").unwrap().unwrap();
        assert_eq!(queued.status, JOB_STATUS_QUEUED);
        assert!(queued.assigned_to.is_none());
    }

    #[tokio::test]
    async fn claim_next_returns_existing_running_job_for_same_agent() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let config_home = temp.path().join("xdg-config");
        std::fs::create_dir_all(&brehon_root).expect("brehon root");
        std::fs::create_dir_all(&config_home).expect("config home");
        write_config_fixture(temp.path());
        let config_home = config_home.to_string_lossy().to_string();
        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", brehon_root.to_str().unwrap()),
            ("XDG_CONFIG_HOME", &config_home),
            ("BREHON_AGENT_NAME", "research-running"),
            ("BREHON_AGENT_ROLE", "research"),
        ]);
        write_research_job_fixture("T-running", "running-1", "specs", JOB_STATUS_RUNNING);
        write_research_job_fixture("T-running", "queued-1", "specs", JOB_STATUS_QUEUED);

        let claim = text_json(
            ResearchTool::new()
                .execute(serde_json::json!({
                    "action": "claim_next",
                    "pool": "specs"
                }))
                .await
                .unwrap(),
        );

        assert_eq!(claim["status"], "claimed");
        assert_eq!(claim["job"]["job_id"], "running-1");
        assert!(claim["message"]
            .as_str()
            .unwrap()
            .contains("already has a running job"));
        assert!(claim["next"].as_str().unwrap().contains("claim_next again"));
        let queued = read_job("T-running", "queued-1").unwrap().unwrap();
        assert_eq!(queued.status, JOB_STATUS_QUEUED);
        assert!(queued.assigned_to.is_none());
    }

    #[tokio::test]
    async fn claim_next_respects_global_max_parallel_jobs() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let config_home = temp.path().join("xdg-config");
        std::fs::create_dir_all(&brehon_root).expect("brehon root");
        std::fs::create_dir_all(&config_home).expect("config home");
        write_config_fixture(temp.path());
        let config_path = temp.path().join(".brehon").join("config.yaml");
        let config = std::fs::read_to_string(&config_path).unwrap().replace(
            "research:\n  enabled: true",
            "research:\n  enabled: true\n  defaults:\n    max_parallel_jobs: 1",
        );
        std::fs::write(&config_path, config).unwrap();
        let config_home = config_home.to_string_lossy().to_string();
        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", brehon_root.to_str().unwrap()),
            ("XDG_CONFIG_HOME", &config_home),
            ("BREHON_AGENT_NAME", "research-1"),
            ("BREHON_AGENT_ROLE", "research"),
        ]);
        write_research_job_fixture("T-global", "running-1", "specs", JOB_STATUS_RUNNING);
        write_research_job_fixture("T-global", "queued-1", "specs", JOB_STATUS_QUEUED);

        let claim = text_json(
            ResearchTool::new()
                .execute(serde_json::json!({
                    "action": "claim_next",
                    "pool": "specs"
                }))
                .await
                .unwrap(),
        );

        assert_eq!(claim["status"], "idle");
        assert!(claim["message"]
            .as_str()
            .unwrap()
            .contains("concurrency limit reached"));
        let queued = read_job("T-global", "queued-1").unwrap().unwrap();
        assert_eq!(queued.status, JOB_STATUS_QUEUED);
        assert!(queued.assigned_to.is_none());
    }

    #[test]
    fn automatic_routes_are_deduplicated_and_non_blocking() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let config_home = temp.path().join("xdg-config");
        std::fs::create_dir_all(&brehon_root).expect("brehon root");
        std::fs::create_dir_all(&config_home).expect("config home");
        write_config_fixture(temp.path());
        write_task_fixture(&brehon_root, "T-route");
        let config_home = config_home.to_string_lossy().to_string();
        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", brehon_root.to_str().unwrap()),
            ("XDG_CONFIG_HOME", &config_home),
            ("BREHON_AGENT_NAME", "supervisor"),
            ("BREHON_AGENT_ROLE", "supervisor"),
        ]);

        let first = run_automatic_routes_for_task(
            "T-route",
            ResearchTrigger::BeforeAssignment,
            "supervisor",
        )
        .expect("first route");
        let second = run_automatic_routes_for_task(
            "T-route",
            ResearchTrigger::BeforeAssignment,
            "supervisor",
        )
        .expect("second route");

        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
        let task: Value = serde_json::from_str(
            &std::fs::read_to_string(brehon_root.join("runtime/tasks/T-route.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(task["status"], "pending");
    }

    #[test]
    fn handoff_renders_compact_research_context() {
        let task = serde_json::json!({
            "task_id": "T-1",
            "research_context": [{
                "artifact_id": "RCH-T-1-specs-001",
                "job_id": "RJOB-T-1-specs-001",
                "pool": "specs",
                "role": "normative_requirements",
                "title": "PFCP map",
                "summary": "Use recovery timestamp and heartbeat requirements.",
                "artifact_path": ".brehon/runtime/research/T-1/RCH-T-1-specs-001/brief.md",
                "structured_path": ".brehon/runtime/research/T-1/RCH-T-1-specs-001/artifact.yaml",
                "created_at": "2026-05-18T00:00:00Z"
            }]
        });
        let rendered = render_task_research_handoff(&task, None);
        assert!(rendered.contains("Research context available"));
        assert!(rendered.contains("PFCP map"));
        assert!(rendered.contains("Treat research artifacts as context"));
    }
}

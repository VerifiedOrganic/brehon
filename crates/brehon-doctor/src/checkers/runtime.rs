//! Runtime diagnostic checker.
//!
//! Detects dead sessions, stale agent states, and runtime directory issues.

use super::Checker;
use crate::types::{DiagnosticCategory, DiagnosticFinding, Severity};
use brehon_types::{load_runtime_stability_counters, StabilityBounds};
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Session staleness threshold in seconds (30 minutes).
const SESSION_STALE_THRESHOLD_SECS: i64 = 1800;

/// Output silence threshold in seconds (10 minutes).
/// Worker has a live heartbeat but no task progress.
const OUTPUT_SILENT_THRESHOLD_SECS: i64 = 600;

/// Checker for runtime issues.
pub struct RuntimeChecker {
    runtime_dir: std::path::PathBuf,
}

impl RuntimeChecker {
    pub fn new(runtime_dir: &Path) -> Self {
        Self {
            runtime_dir: runtime_dir.to_path_buf(),
        }
    }

    fn check_stale_sessions(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let sessions_dir = self.runtime_dir.join("sessions");

        if !sessions_dir.exists() {
            return Ok(findings);
        }

        for entry in std::fs::read_dir(&sessions_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }

            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(name) = json.get("name").and_then(|v| v.as_str()) {
                        let last_seen = json
                            .get("last_seen_at")
                            .or_else(|| json.get("registered_at"))
                            .and_then(|v| v.as_str())
                            .and_then(|s| {
                                DateTime::parse_from_rfc3339(s)
                                    .ok()
                                    .map(|dt| dt.with_timezone(&Utc))
                            });

                        if let Some(seen) = last_seen {
                            let now = Utc::now();
                            let elapsed = now.signed_duration_since(seen);

                            if elapsed.num_seconds() > SESSION_STALE_THRESHOLD_SECS {
                                let agent_name = name.to_string();
                                findings.push(
                                    DiagnosticFinding::new(
                                        DiagnosticCategory::Runtime,
                                        Severity::Warning,
                                        format!("Stale session: {}", agent_name),
                                    )
                                    .with_subject(agent_name.clone())
                                    .with_description(format!(
                                        "Last seen {} seconds ago",
                                        elapsed.num_seconds()
                                    ))
                                    .with_suggestion(
                                        "Agent may have crashed. Session file can be cleaned up.",
                                    ),
                                );
                            }
                        }
                    }
                }
            }
        }

        Ok(findings)
    }

    fn check_dead_panes(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let panes_dir = self.runtime_dir.join("panes");

        if !panes_dir.exists() {
            return Ok(findings);
        }

        for entry in std::fs::read_dir(&panes_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }

            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(pid) = json.get("pid").and_then(|v| v.as_u64()) {
                        #[cfg(unix)]
                        {
                            let pid_i32 = pid as i32;
                            let result = unsafe { libc::kill(pid_i32, 0) };
                            if result != 0
                                && std::io::Error::last_os_error().raw_os_error()
                                    == Some(libc::ESRCH)
                            {
                                let pane_id =
                                    json.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
                                findings.push(
                                    DiagnosticFinding::new(
                                        DiagnosticCategory::Runtime,
                                        Severity::Error,
                                        format!("Dead pane: {} (PID {})", pane_id, pid),
                                    )
                                    .with_subject(pane_id.to_string())
                                    .with_suggestion("Process is dead. Clean up the pane file."),
                                );
                            }
                        }
                        #[cfg(not(unix))]
                        {
                            let _ = pid;
                        }
                    }
                }
            }
        }

        Ok(findings)
    }

    fn check_missing_directories(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let required_dirs = ["tasks", "sessions"];

        for dir_name in required_dirs {
            let dir = self.runtime_dir.join(dir_name);
            if !dir.exists() {
                findings.push(
                    DiagnosticFinding::new(
                        DiagnosticCategory::Runtime,
                        Severity::Error,
                        format!("Missing required directory: {}", dir_name),
                    )
                    .with_subject(dir.display().to_string())
                    .with_suggestion(format!("Create directory with: mkdir -p {}", dir.display())),
                );
            }
        }

        Ok(findings)
    }

    fn check_stale_prompt_queue(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let prompt_queue_dir = self.runtime_dir.join("prompt-queue");

        if !prompt_queue_dir.exists() {
            return Ok(findings);
        }

        let sessions_dir = self.runtime_dir.join("sessions");
        let mut live_sessions = std::collections::HashSet::new();

        if sessions_dir.exists() {
            for entry in std::fs::read_dir(&sessions_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "json") {
                    continue;
                }

                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(name) = json.get("name").and_then(|v| v.as_str()) {
                            live_sessions.insert(name.to_string());
                        }
                    }
                }
            }
        }

        for entry in std::fs::read_dir(&prompt_queue_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "prompt") {
                continue;
            }

            // Prompt files are named: {agent_name}.prompt or {agent_name}-{suffix}.prompt
            // The agent_name may contain hyphens, so we check all live sessions for a match
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                let has_live_session = live_sessions.iter().any(|session_name| {
                    stem.starts_with(session_name)
                        && (stem.len() == session_name.len()
                            || stem.as_bytes().get(session_name.len()) == Some(&b'-'))
                });

                if !has_live_session {
                    findings.push(
                        DiagnosticFinding::new(
                            DiagnosticCategory::Runtime,
                            Severity::Warning,
                            format!("Stale prompt-queue file: {}", stem),
                        )
                        .with_subject(stem.to_string())
                        .with_description("Prompt file exists but no matching active session found")
                        .with_suggestion(
                            "Remove stale prompt files: rm runtime/prompt-queue/*.prompt",
                        ),
                    );
                }
            }
        }

        Ok(findings)
    }

    /// Detect workers that are heartbeat-live but output-silent.
    ///
    /// A worker is output-silent when their session is recent (heartbeat alive)
    /// but their assigned task hasn't been updated within the output threshold.
    fn check_output_silent_workers(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let sessions_dir = self.runtime_dir.join("sessions");
        let tasks_dir = self.runtime_dir.join("tasks");

        if !sessions_dir.exists() || !tasks_dir.exists() {
            return Ok(findings);
        }

        let now = Utc::now();

        // Load all tasks indexed by assignee
        let mut tasks_by_assignee: std::collections::HashMap<String, Vec<serde_json::Value>> =
            std::collections::HashMap::new();
        if let Ok(entries) = std::fs::read_dir(&tasks_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(assignee) = json.get("assignee").and_then(|v| v.as_str()) {
                            let status = json
                                .get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            // Only consider active tasks
                            if status == "in_progress" || status == "assigned" {
                                tasks_by_assignee
                                    .entry(assignee.to_string())
                                    .or_default()
                                    .push(json);
                            }
                        }
                    }
                }
            }
        }

        // Check each worker session
        for entry in std::fs::read_dir(&sessions_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }

            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                    let role = json.get("role").and_then(|v| v.as_str()).unwrap_or("");
                    if role != "worker" {
                        continue;
                    }

                    let name = match json.get("name").and_then(|v| v.as_str()) {
                        Some(n) => n,
                        None => continue,
                    };

                    let last_seen = parse_timestamp(&json, &["last_seen_at"]);
                    let session_started = parse_timestamp(&json, &["registered_at", "started_at"]);

                    let heartbeat_live = last_seen
                        .map(|seen| (now - seen).num_seconds() < SESSION_STALE_THRESHOLD_SECS)
                        .unwrap_or(false);

                    if !heartbeat_live {
                        continue; // Stale sessions are caught by check_stale_sessions
                    }

                    // Check if worker has active tasks with stale updates
                    if let Some(tasks) = tasks_by_assignee.get(name) {
                        for task in tasks {
                            let task_id =
                                task.get("task_id").and_then(|v| v.as_str()).unwrap_or("?");

                            let task_updated = parse_timestamp(task, &["updated_at"]);

                            if let Some(updated) = task_updated {
                                let effective_updated =
                                    session_started.map_or(updated, |started| updated.max(started));
                                let idle_secs = (now - effective_updated).num_seconds();
                                if idle_secs > OUTPUT_SILENT_THRESHOLD_SECS {
                                    let idle_mins = idle_secs / 60;
                                    findings.push(
                                        DiagnosticFinding::new(
                                            DiagnosticCategory::Runtime,
                                            Severity::Warning,
                                            format!(
                                                "Output-silent worker: {} on task {}",
                                                name, task_id
                                            ),
                                        )
                                        .with_subject(name.to_string())
                                        .with_description(format!(
                                            "Worker heartbeat is live but task {} has not been updated for {} minutes",
                                            task_id, idle_mins
                                        ))
                                        .with_suggestion("Consider sending nudge"),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(findings)
    }

    /// Detect nudges that were delivered but not acknowledged beyond threshold.
    fn check_unacknowledged_nudges(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let tasks_dir = self.runtime_dir.join("tasks");

        if !tasks_dir.exists() {
            return Ok(findings);
        }

        let now = Utc::now();

        // Scan task files for nudge-related notes
        for entry in std::fs::read_dir(&tasks_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }

            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                    let task_id = json.get("task_id").and_then(|v| v.as_str()).unwrap_or("?");
                    let assignee = json.get("assignee").and_then(|v| v.as_str()).unwrap_or("?");
                    let status = json
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");

                    // Only check active tasks
                    if status != "in_progress" && status != "assigned" {
                        continue;
                    }

                    if assignee == "?" || assignee.trim().is_empty() {
                        continue;
                    }

                    // Check for stalled tasks with no progress
                    let percent = json.get("percent").and_then(|v| v.as_u64()).unwrap_or(0);
                    let updated_at = parse_timestamp(&json, &["updated_at"]);
                    let session_path = self
                        .runtime_dir
                        .join("sessions")
                        .join(format!("{assignee}.json"));
                    let session_started = std::fs::read_to_string(&session_path)
                        .ok()
                        .and_then(|content| {
                            serde_json::from_str::<serde_json::Value>(&content).ok()
                        })
                        .and_then(|session| {
                            parse_timestamp(&session, &["registered_at", "started_at"])
                        });

                    if let Some(updated) = updated_at {
                        let effective_updated =
                            session_started.map_or(updated, |started| updated.max(started));
                        let stale_mins = (now - effective_updated).num_seconds() / 60;
                        // If task hasn't been updated in 20+ minutes and is actively assigned
                        if stale_mins > 20 && percent > 0 && percent < 100 {
                            findings.push(
                                DiagnosticFinding::new(
                                    DiagnosticCategory::Runtime,
                                    Severity::Warning,
                                    format!(
                                        "Stalled task {} assigned to {} ({}% for {} min)",
                                        task_id, assignee, percent, stale_mins
                                    ),
                                )
                                .with_subject(task_id.to_string())
                                .with_description(format!(
                                    "Task at {}% progress with no updates for {} minutes",
                                    percent, stale_mins
                                ))
                                .with_suggestion(format!(
                                    "Nudge unacknowledged for {} minutes — consider reassignment",
                                    stale_mins
                                )),
                            );
                        }
                    }
                }
            }
        }

        Ok(findings)
    }

    /// Read a persisted `StabilityCounters` snapshot from the runtime directory
    /// and flag any values that exceed soft bounds.
    ///
    /// The counters file (`stability-counters.json`) is written by the
    /// orchestrator or TUI on each tick. If the file is missing or stale,
    /// the check is skipped rather than emitting a false alarm.
    fn check_stability_bounds(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let counters_path = self.runtime_dir.join("stability-counters.json");

        if !counters_path.exists() {
            return Ok(findings);
        }

        let Some(counters) = load_runtime_stability_counters(&counters_path) else {
            return Ok(findings);
        };

        let bounds = StabilityBounds::default();

        if counters.pending_requests > bounds.max_pending_requests {
            findings.push(
                DiagnosticFinding::new(
                    DiagnosticCategory::Runtime,
                    Severity::Warning,
                    format!(
                        "pending_requests ({}) exceeds bound ({})",
                        counters.pending_requests, bounds.max_pending_requests
                    ),
                )
                .with_subject("pending_requests".to_string())
                .with_suggestion("Check for stuck prompt responses or leaked request handles"),
            );
        }

        if counters.pending_prompt_waiters > bounds.max_prompt_results {
            findings.push(
                DiagnosticFinding::new(
                    DiagnosticCategory::Runtime,
                    Severity::Warning,
                    format!(
                        "prompt_results ({}) exceeds bound ({})",
                        counters.pending_prompt_waiters, bounds.max_prompt_results
                    ),
                )
                .with_subject("prompt_results".to_string())
                .with_suggestion("Results are accumulating without being consumed"),
            );
        }

        if counters.active_reviews > bounds.max_active_reviews {
            findings.push(
                DiagnosticFinding::new(
                    DiagnosticCategory::Runtime,
                    Severity::Warning,
                    format!(
                        "active_reviews ({}) exceeds bound ({})",
                        counters.active_reviews, bounds.max_active_reviews
                    ),
                )
                .with_subject("active_reviews".to_string())
                .with_suggestion("Reviews may be stuck; check for stale or unresponsive reviewers"),
            );
        }

        if counters.blocked_sends > bounds.max_blocked_sends {
            findings.push(
                DiagnosticFinding::new(
                    DiagnosticCategory::Runtime,
                    Severity::Error,
                    format!(
                        "blocked_sends ({}) exceeds bound ({})",
                        counters.blocked_sends, bounds.max_blocked_sends
                    ),
                )
                .with_subject("blocked_sends".to_string())
                .with_suggestion("Agent subprocess may have crashed; check session health"),
            );
        }

        // Emit an informational finding with current counter values even when
        // within bounds, so operators can monitor trends without re-running
        // the doctor.
        if findings.is_empty() {
            findings.push(
                DiagnosticFinding::new(
                    DiagnosticCategory::Runtime,
                    Severity::Info,
                    format!(
                        "Stability counters: pending={} results={} reviews={} completed={} assignments={} blocked={} tokens={}",
                        counters.pending_requests,
                        counters.pending_prompt_waiters,
                        counters.active_reviews,
                        counters.completed_tasks,
                        counters.assignment_history,
                        counters.blocked_sends,
                        counters.tokens_used,
                    ),
                )
                .with_subject("stability_counters".to_string()),
            );
        }

        Ok(findings)
    }

    fn check_mcp_server_metadata(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let servers_dir = self.runtime_dir.join("mcp-servers");
        if !servers_dir.exists() {
            return Ok(findings);
        }
        let brehon_root = self.runtime_dir.parent().unwrap_or(&self.runtime_dir);
        let project_root = brehon_root.parent().unwrap_or(brehon_root);
        let current_revision = current_source_revision(project_root);

        for entry in std::fs::read_dir(&servers_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(metadata) = serde_json::from_str::<serde_json::Value>(&content) else {
                continue;
            };
            let pid = metadata
                .get("pid")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            if pid == 0 {
                continue;
            }
            if !pid_alive(pid as u32) {
                findings.push(
                    DiagnosticFinding::new(
                        DiagnosticCategory::Runtime,
                        Severity::Error,
                        format!("Dead MCP server metadata: pid {pid}"),
                    )
                    .with_subject(path.display().to_string())
                    .with_suggestion("Run `brehon doctor --repair` to remove stale MCP metadata."),
                );
                continue;
            }

            if metadata
                .get("server_version")
                .and_then(|value| value.as_str())
                != Some(env!("CARGO_PKG_VERSION"))
            {
                findings.push(
                    DiagnosticFinding::new(
                        DiagnosticCategory::Runtime,
                        Severity::Warning,
                        format!("Stale MCP server version: pid {pid}"),
                    )
                    .with_subject(format!("pid {pid}"))
                    .with_suggestion("Restart the live `brehon serve` MCP process."),
                );
            }

            if let Some(binary_path) = metadata
                .get("binary_path")
                .and_then(|value| value.as_str())
                .map(PathBuf::from)
            {
                let recorded = metadata
                    .get("binary_modified_unix_secs")
                    .and_then(|value| value.as_u64());
                if recorded.is_some() && file_modified_unix_secs(&binary_path) != recorded {
                    findings.push(
                        DiagnosticFinding::new(
                            DiagnosticCategory::Runtime,
                            Severity::Warning,
                            format!("Stale installed Brehon binary: pid {pid}"),
                        )
                        .with_subject(binary_path.display().to_string())
                        .with_suggestion("Restart the live `brehon serve` MCP process."),
                    );
                }
            }

            if let (Some(recorded), Some(current)) = (
                metadata
                    .get("source_revision")
                    .and_then(|value| value.as_str()),
                current_revision.as_deref(),
            ) {
                if recorded != current {
                    findings.push(
                        DiagnosticFinding::new(
                            DiagnosticCategory::Runtime,
                            Severity::Warning,
                            format!("Stale MCP source revision: pid {pid}"),
                        )
                        .with_subject(format!("pid {pid}"))
                        .with_description(format!(
                            "serve started at {recorded}, current source is {current}"
                        ))
                        .with_suggestion("Restart the live `brehon serve` MCP process."),
                    );
                }
            }
        }

        Ok(findings)
    }
}

fn parse_timestamp(json: &serde_json::Value, fields: &[&str]) -> Option<DateTime<Utc>> {
    fields
        .iter()
        .find_map(|field| json.get(*field).and_then(|v| v.as_str()))
        .and_then(|s| {
            DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        })
}

impl Checker for RuntimeChecker {
    fn category(&self) -> DiagnosticCategory {
        DiagnosticCategory::Runtime
    }

    fn check(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        findings.extend(self.check_stale_sessions()?);
        findings.extend(self.check_dead_panes()?);
        findings.extend(self.check_missing_directories()?);
        findings.extend(self.check_stale_prompt_queue()?);
        findings.extend(self.check_output_silent_workers()?);
        findings.extend(self.check_unacknowledged_nudges()?);
        findings.extend(self.check_stability_bounds()?);
        findings.extend(self.check_mcp_server_metadata()?);
        Ok(findings)
    }
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as i32, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

fn current_source_revision(project_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(project_root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn file_modified_unix_secs(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_checker() {
        let checker = RuntimeChecker::new(Path::new("/tmp"));
        assert_eq!(checker.category(), DiagnosticCategory::Runtime);
    }

    #[test]
    fn test_output_silent_worker_detected() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        let sessions_dir = runtime.join("sessions");
        let tasks_dir = runtime.join("tasks");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let now = Utc::now();

        // Worker session with recent heartbeat
        let session = serde_json::json!({
            "name": "worker-1",
            "role": "worker",
            "session_id": "sess-1",
            "last_seen_at": now.to_rfc3339()
        });
        std::fs::write(
            sessions_dir.join("worker-1.json"),
            serde_json::to_string(&session).unwrap(),
        )
        .unwrap();

        // Task with stale updated_at (20 minutes ago)
        let stale_time = now - chrono::Duration::seconds(1200);
        let task = serde_json::json!({
            "task_id": "T-stale",
            "assignee": "worker-1",
            "status": "in_progress",
            "updated_at": stale_time.to_rfc3339()
        });
        std::fs::write(
            tasks_dir.join("T-stale.json"),
            serde_json::to_string(&task).unwrap(),
        )
        .unwrap();

        let checker = RuntimeChecker::new(&runtime);
        let findings = checker.check_output_silent_workers().unwrap();
        assert_eq!(findings.len(), 1);
        assert!(findings[0].summary.contains("Output-silent"));
        assert!(findings[0].summary.contains("worker-1"));
        assert_eq!(
            findings[0].suggestion.as_deref(),
            Some("Consider sending nudge")
        );
    }

    #[test]
    fn test_active_worker_not_flagged() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        let sessions_dir = runtime.join("sessions");
        let tasks_dir = runtime.join("tasks");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let now = Utc::now();

        let session = serde_json::json!({
            "name": "worker-1",
            "role": "worker",
            "session_id": "sess-1",
            "last_seen_at": now.to_rfc3339()
        });
        std::fs::write(
            sessions_dir.join("worker-1.json"),
            serde_json::to_string(&session).unwrap(),
        )
        .unwrap();

        // Task with fresh updated_at
        let task = serde_json::json!({
            "task_id": "T-active",
            "assignee": "worker-1",
            "status": "in_progress",
            "updated_at": now.to_rfc3339()
        });
        std::fs::write(
            tasks_dir.join("T-active.json"),
            serde_json::to_string(&task).unwrap(),
        )
        .unwrap();

        let checker = RuntimeChecker::new(&runtime);
        let findings = checker.check_output_silent_workers().unwrap();
        assert!(findings.is_empty(), "Active worker should not be flagged");
    }

    #[test]
    fn test_recovered_worker_not_flagged_from_pre_restart_task_timestamp() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        let sessions_dir = runtime.join("sessions");
        let tasks_dir = runtime.join("tasks");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let now = Utc::now();
        let session_started = now - chrono::Duration::minutes(2);
        let stale_task_time = now - chrono::Duration::minutes(55);

        let session = serde_json::json!({
            "name": "worker-1",
            "role": "worker",
            "session_id": "sess-1",
            "registered_at": session_started.to_rfc3339(),
            "last_seen_at": now.to_rfc3339()
        });
        std::fs::write(
            sessions_dir.join("worker-1.json"),
            serde_json::to_string(&session).unwrap(),
        )
        .unwrap();

        let task = serde_json::json!({
            "task_id": "T-recovered",
            "assignee": "worker-1",
            "status": "in_progress",
            "updated_at": stale_task_time.to_rfc3339()
        });
        std::fs::write(
            tasks_dir.join("T-recovered.json"),
            serde_json::to_string(&task).unwrap(),
        )
        .unwrap();

        let checker = RuntimeChecker::new(&runtime);
        let findings = checker.check_output_silent_workers().unwrap();
        assert!(
            findings.is_empty(),
            "Recovered task should not be flagged from a pre-restart timestamp"
        );
    }

    #[test]
    fn test_stalled_task_detected() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        let tasks_dir = runtime.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let now = Utc::now();
        let stale_time = now - chrono::Duration::seconds(1800); // 30 min ago

        let task = serde_json::json!({
            "task_id": "T-stalled",
            "assignee": "worker-stuck",
            "status": "in_progress",
            "percent": 50,
            "updated_at": stale_time.to_rfc3339()
        });
        std::fs::write(
            tasks_dir.join("T-stalled.json"),
            serde_json::to_string(&task).unwrap(),
        )
        .unwrap();

        let checker = RuntimeChecker::new(&runtime);
        let findings = checker.check_unacknowledged_nudges().unwrap();
        assert_eq!(findings.len(), 1);
        assert!(findings[0].summary.contains("Stalled task"));
        assert!(findings[0].summary.contains("T-stalled"));
        assert!(findings[0]
            .suggestion
            .as_deref()
            .unwrap()
            .contains("consider reassignment"));
    }

    #[test]
    fn test_recovered_stalled_task_not_flagged_until_current_session_ages() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        let sessions_dir = runtime.join("sessions");
        let tasks_dir = runtime.join("tasks");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let now = Utc::now();
        let session_started = now - chrono::Duration::minutes(3);
        let stale_task_time = now - chrono::Duration::minutes(55);

        let session = serde_json::json!({
            "name": "worker-1",
            "role": "worker",
            "session_id": "sess-1",
            "registered_at": session_started.to_rfc3339(),
            "last_seen_at": now.to_rfc3339()
        });
        std::fs::write(
            sessions_dir.join("worker-1.json"),
            serde_json::to_string(&session).unwrap(),
        )
        .unwrap();

        let task = serde_json::json!({
            "task_id": "T-recovered-stall",
            "assignee": "worker-1",
            "status": "in_progress",
            "percent": 40,
            "updated_at": stale_task_time.to_rfc3339()
        });
        std::fs::write(
            tasks_dir.join("T-recovered-stall.json"),
            serde_json::to_string(&task).unwrap(),
        )
        .unwrap();

        let checker = RuntimeChecker::new(&runtime);
        let findings = checker.check_unacknowledged_nudges().unwrap();
        assert!(
            findings.is_empty(),
            "Recovered task should not be flagged stalled until the current session ages"
        );
    }

    #[test]
    fn test_in_progress_task_without_assignee_is_not_flagged_stalled() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        let tasks_dir = runtime.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let now = Utc::now() - chrono::Duration::minutes(30);
        let task = serde_json::json!({
            "task_id": "T-unowned",
            "status": "in_progress",
            "percent": 30,
            "updated_at": now.to_rfc3339()
        });
        std::fs::write(
            tasks_dir.join("T-unowned.json"),
            serde_json::to_string(&task).unwrap(),
        )
        .unwrap();

        let checker = RuntimeChecker::new(&runtime);
        let findings = checker.check_unacknowledged_nudges().unwrap();
        assert!(
            findings.is_empty(),
            "Unassigned in-progress task should not be flagged as a stalled worker task"
        );
    }

    #[test]
    fn test_missing_directories_does_not_require_legacy_events_dir() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        std::fs::create_dir_all(runtime.join("tasks")).unwrap();
        std::fs::create_dir_all(runtime.join("sessions")).unwrap();

        let checker = RuntimeChecker::new(&runtime);
        let findings = checker.check_missing_directories().unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn test_stability_bounds_no_file() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();

        let checker = RuntimeChecker::new(&runtime);
        let findings = checker.check_stability_bounds().unwrap();
        assert!(
            findings.is_empty(),
            "No counters file should produce no findings"
        );
    }

    #[test]
    fn test_stability_bounds_within_bounds() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();

        let counters = brehon_types::StabilityCounters {
            pending_requests: 5,
            pending_prompt_waiters: 10,
            active_reviews: 3,
            completed_tasks: 100,
            assignment_history: 50,
            blocked_sends: 0,
            tokens_used: 1_024,
        };
        std::fs::write(
            runtime.join("stability-counters.json"),
            serde_json::to_string(&counters).unwrap(),
        )
        .unwrap();

        let checker = RuntimeChecker::new(&runtime);
        let findings = checker.check_stability_bounds().unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Info);
        assert!(findings[0].summary.contains("Stability counters"));
    }

    #[test]
    fn test_stability_bounds_reads_live_runtime_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let brehon_root = root.path();
        let runtime = brehon_root.join("runtime");
        std::fs::create_dir_all(runtime.join("tasks")).unwrap();
        std::fs::create_dir_all(runtime.join("reviews/T-review")).unwrap();

        brehon_types::write_session_stability_counters(
            brehon_root,
            "session-1",
            brehon_types::StabilityCounters {
                pending_requests: 4,
                pending_prompt_waiters: 2,
                tokens_used: 2_048,
                ..Default::default()
            },
        )
        .unwrap();
        brehon_types::increment_assignment_history(brehon_root, 3).unwrap();
        std::fs::write(
            runtime.join("tasks/T-merged.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": "T-merged",
                "status": "merged"
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            runtime.join("reviews/T-review/state.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": "T-review",
                "status": "collecting"
            }))
            .unwrap(),
        )
        .unwrap();
        brehon_types::refresh_runtime_stability_counters(brehon_root).unwrap();

        let checker = RuntimeChecker::new(&runtime);
        let findings = checker.check_stability_bounds().unwrap();
        assert_eq!(findings.len(), 1);
        assert!(findings[0].summary.contains("pending=4"));
        assert!(findings[0].summary.contains("reviews=1"));
        assert!(findings[0].summary.contains("completed=1"));
        assert!(findings[0].summary.contains("assignments=3"));
        assert!(findings[0].summary.contains("tokens=2048"));
    }

    #[test]
    fn test_stability_bounds_exceeded() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();

        let counters = brehon_types::StabilityCounters {
            pending_requests: 300,
            pending_prompt_waiters: 10,
            active_reviews: 3,
            completed_tasks: 100,
            assignment_history: 50,
            blocked_sends: 10,
            tokens_used: 4_096,
        };
        std::fs::write(
            runtime.join("stability-counters.json"),
            serde_json::to_string(&counters).unwrap(),
        )
        .unwrap();

        let checker = RuntimeChecker::new(&runtime);
        let findings = checker.check_stability_bounds().unwrap();
        assert!(
            findings.len() >= 2,
            "Should flag pending_requests and blocked_sends"
        );
        assert!(findings
            .iter()
            .any(|f| f.summary.contains("pending_requests")));
        assert!(findings.iter().any(|f| f.summary.contains("blocked_sends")));
    }

    #[test]
    fn test_stability_bounds_invalid_json() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();

        std::fs::write(runtime.join("stability-counters.json"), "not json").unwrap();

        let checker = RuntimeChecker::new(&runtime);
        let findings = checker.check_stability_bounds().unwrap();
        assert!(
            findings.is_empty(),
            "Invalid JSON should produce no findings"
        );
    }
}

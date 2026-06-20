//! Deterministic repair actions for local Brehon runtime state.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const STALE_LOCK_AFTER: Duration = Duration::from_secs(30);
const SESSION_STALE_AFTER_SECS: i64 = 30 * 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepairAction {
    pub code: String,
    pub subject: String,
    pub repaired: bool,
    pub message: String,
}

impl RepairAction {
    fn repaired(
        code: impl Into<String>,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            subject: subject.into(),
            repaired: true,
            message: message.into(),
        }
    }

    fn skipped(
        code: impl Into<String>,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            subject: subject.into(),
            repaired: false,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepairReport {
    pub actions: Vec<RepairAction>,
    pub repaired_count: usize,
    pub skipped_count: usize,
}

impl RepairReport {
    fn push(&mut self, action: RepairAction) {
        if action.repaired {
            self.repaired_count += 1;
        } else {
            self.skipped_count += 1;
        }
        self.actions.push(action);
    }

    pub fn has_repairs(&self) -> bool {
        self.repaired_count > 0
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

impl fmt::Display for RepairReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "BREHON DOCTOR REPAIR")?;
        writeln!(
            f,
            "repaired={} skipped={}",
            self.repaired_count, self.skipped_count
        )?;
        if self.actions.is_empty() {
            writeln!(f, "No repairable issues found.")?;
            return Ok(());
        }
        for action in &self.actions {
            writeln!(
                f,
                "- {} [{}] {}",
                if action.repaired {
                    "repaired"
                } else {
                    "skipped"
                },
                action.code,
                action.subject
            )?;
            writeln!(f, "  {}", action.message)?;
        }
        Ok(())
    }
}

pub fn run_repair(brehon_root: &Path) -> RepairReport {
    let mut report = RepairReport::default();
    let runtime_dir = brehon_root.join("runtime");

    repair_stale_locks(&runtime_dir, &mut report);
    repair_dead_mcp_server_metadata(&runtime_dir, &mut report);
    repair_orphaned_workers(&runtime_dir, &mut report);
    repair_impossible_task_states(&runtime_dir, &mut report);
    report_stale_mcp_processes(brehon_root, &runtime_dir, &mut report);
    report_bad_routing_lanes(brehon_root, &mut report);

    report
}

fn repair_stale_locks(runtime_dir: &Path, report: &mut RepairReport) {
    let mut candidates = Vec::new();
    candidates.push(runtime_dir.join(".repo.lock"));
    let tasks_dir = runtime_dir.join("tasks");
    if let Ok(entries) = std::fs::read_dir(&tasks_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.') && name.ends_with(".lock"))
            {
                candidates.push(path);
            }
        }
    }

    for path in candidates {
        if !path.exists() {
            continue;
        }
        let stale = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= STALE_LOCK_AFTER);
        if !stale {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => report.push(RepairAction::repaired(
                "stale_lock_removed",
                path.display().to_string(),
                "Removed stale Brehon lock file.",
            )),
            Err(err) => report.push(RepairAction::skipped(
                "stale_lock_remove_failed",
                path.display().to_string(),
                format!("Could not remove stale lock: {err}"),
            )),
        }
    }
}

fn session_timestamp(session: &Value) -> Option<DateTime<Utc>> {
    session
        .get("last_seen_at")
        .and_then(|value| value.as_str())
        .or_else(|| {
            session
                .get("registered_at")
                .and_then(|value| value.as_str())
        })
        .or_else(|| session.get("started_at").and_then(|value| value.as_str()))
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn session_is_live(session: &Value) -> bool {
    session_timestamp(session)
        .map(|seen| {
            Utc::now().signed_duration_since(seen).num_seconds() <= SESSION_STALE_AFTER_SECS
        })
        .unwrap_or(true)
}

fn live_worker_names(runtime_dir: &Path) -> HashSet<String> {
    let sessions_dir = runtime_dir.join("sessions");
    let mut names = HashSet::new();
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return names;
    };
    for entry in entries.flatten() {
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(session) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        if session.get("role").and_then(|value| value.as_str()) != Some("worker") {
            continue;
        }
        if !session_is_live(&session) {
            continue;
        }
        if let Some(name) = session.get("name").and_then(|value| value.as_str()) {
            names.insert(name.to_string());
        }
    }
    names
}

fn active_worker_status(status: &str) -> bool {
    matches!(
        status,
        "assigned"
            | "in_progress"
            | "changes_requested"
            | "blocked"
            | "Assigned"
            | "InProgress"
            | "ChangesRequested"
            | "Blocked"
    )
}

fn task_has_manual_blockers(task: &Value) -> bool {
    task.get("blockers")
        .and_then(|value| value.as_str())
        .is_some_and(|value| !value.trim().is_empty())
        || task
            .get("blocked_by")
            .and_then(|value| value.as_array())
            .is_some_and(|items| !items.is_empty())
}

fn repair_orphaned_workers(runtime_dir: &Path, report: &mut RepairReport) {
    let live_workers = live_worker_names(runtime_dir);
    let tasks_dir = runtime_dir.join("tasks");
    let Ok(entries) = std::fs::read_dir(&tasks_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut task) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        let status = task
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("pending")
            .to_string();
        if !active_worker_status(&status) {
            continue;
        }
        let Some(assignee) = task
            .get("assignee")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
        else {
            continue;
        };
        if live_workers.contains(&assignee) {
            continue;
        }
        let task_id = task
            .get("task_id")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
            .to_string();
        task["orphaned_assignee"] = Value::String(assignee.clone());
        task["orphaned_status"] = Value::String(status.clone());
        task["assignee"] = Value::Null;
        task["inbox_delivered"] = Value::Bool(false);
        if let Some(object) = task.as_object_mut() {
            object.remove("activity");
        }
        let next_status = if status.eq_ignore_ascii_case("changes_requested") {
            "changes_requested"
        } else if task_has_manual_blockers(&task) {
            "blocked"
        } else {
            "pending"
        };
        task["status"] = Value::String(next_status.to_string());
        task["recovery_note"] = Value::String(format!(
            "Doctor repair recovered orphaned task from {status}; previous assignee {assignee} is not live."
        ));
        task["updated_at"] = Value::String(Utc::now().to_rfc3339());
        match brehon_types::write_json_atomic(&path, &task) {
            Ok(()) => report.push(RepairAction::repaired(
                "orphaned_worker_recovered",
                task_id,
                format!("Cleared dead assignee {assignee}; task returned to {next_status}."),
            )),
            Err(err) => report.push(RepairAction::skipped(
                "orphaned_worker_repair_failed",
                task_id,
                format!("Could not write repaired task: {err}"),
            )),
        }
    }
}

fn review_state_exists(runtime_dir: &Path, task_id: &str) -> bool {
    runtime_dir
        .join("reviews")
        .join(task_id)
        .join("state.json")
        .exists()
}

fn repair_impossible_task_states(runtime_dir: &Path, report: &mut RepairReport) {
    let tasks_dir = runtime_dir.join("tasks");
    let Ok(entries) = std::fs::read_dir(&tasks_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut task) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        let task_id = task
            .get("task_id")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
            .to_string();
        let status = task
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("pending")
            .to_string();
        let mut repaired = false;
        let mut message = String::new();
        if status == "in_review" && !review_state_exists(runtime_dir, &task_id) {
            let has_commit = task
                .get("latest_commit")
                .and_then(|value| value.as_str())
                .is_some_and(|value| !value.trim().is_empty());
            let next = if has_commit {
                "review_ready"
            } else {
                "pending"
            };
            task["status"] = Value::String(next.to_string());
            task["review_repair_note"] = Value::String(
                "Doctor repair recovered in_review task with missing review state.".to_string(),
            );
            repaired = true;
            message = format!("Moved in_review task with missing review state to {next}.");
        } else if active_worker_status(&status)
            && task
                .get("assignee")
                .and_then(|value| value.as_str())
                .is_none_or(|value| value.trim().is_empty())
            && !matches!(status.as_str(), "blocked" | "changes_requested")
        {
            task["status"] = Value::String("pending".to_string());
            task["recovery_note"] =
                Value::String("Doctor repair recovered active task with no assignee.".to_string());
            repaired = true;
            message = "Moved active task with no assignee back to pending.".to_string();
        }
        if !repaired {
            continue;
        }
        task["updated_at"] = Value::String(Utc::now().to_rfc3339());
        match brehon_types::write_json_atomic(&path, &task) {
            Ok(()) => report.push(RepairAction::repaired(
                "impossible_task_state_repaired",
                task_id,
                message,
            )),
            Err(err) => report.push(RepairAction::skipped(
                "impossible_task_state_repair_failed",
                task_id,
                format!("Could not write repaired task: {err}"),
            )),
        }
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

fn repair_dead_mcp_server_metadata(runtime_dir: &Path, report: &mut RepairReport) {
    let dir = runtime_dir.join("mcp-servers");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(metadata) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        let Some(pid) = metadata.get("pid").and_then(|value| value.as_u64()) else {
            continue;
        };
        if pid_alive(pid as u32) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => report.push(RepairAction::repaired(
                "dead_mcp_server_metadata_removed",
                path.display().to_string(),
                format!("Removed metadata for dead brehon serve MCP process pid {pid}."),
            )),
            Err(err) => report.push(RepairAction::skipped(
                "dead_mcp_server_metadata_remove_failed",
                path.display().to_string(),
                format!("Could not remove dead MCP server metadata: {err}"),
            )),
        }
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

fn report_stale_mcp_processes(brehon_root: &Path, runtime_dir: &Path, report: &mut RepairReport) {
    let dir = runtime_dir.join("mcp-servers");
    let project_root = brehon_root.parent().unwrap_or(brehon_root);
    let current_revision = current_source_revision(project_root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(metadata) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        let pid = metadata
            .get("pid")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        if pid == 0 || !pid_alive(pid as u32) {
            continue;
        }
        if metadata
            .get("server_version")
            .and_then(|value| value.as_str())
            != Some(env!("CARGO_PKG_VERSION"))
        {
            report.push(RepairAction::skipped(
                "stale_mcp_server_version",
                format!("pid {pid}"),
                "Live brehon serve was started by a different Brehon version; restart the MCP server.",
            ));
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
                report.push(RepairAction::skipped(
                    "stale_installed_binary",
                    binary_path.display().to_string(),
                    "Live brehon serve binary has changed on disk; restart the MCP server.",
                ));
            }
        }
        if let (Some(recorded), Some(current)) = (
            metadata
                .get("source_revision")
                .and_then(|value| value.as_str()),
            current_revision.as_deref(),
        ) {
            if recorded != current {
                report.push(RepairAction::skipped(
                    "stale_source_revision",
                    format!("pid {pid}"),
                    "Live brehon serve was started from an older source revision; restart the MCP server.",
                ));
            }
        }
    }
}

fn report_bad_routing_lanes(brehon_root: &Path, report: &mut RepairReport) {
    let project_root = brehon_root.parent().unwrap_or(brehon_root);
    let Ok((_config, warnings)) = brehon_config::load_config_for_diagnostics(Some(project_root))
    else {
        return;
    };
    for warning in warnings {
        if warning.kind == brehon_config::ValidationWarningKind::RoutingPolicyConflict {
            report.push(RepairAction::skipped(
                "bad_routing_lane",
                "routing".to_string(),
                warning.message,
            ));
        }
    }
}

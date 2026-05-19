//! Task diagnostic checker.
//!
//! Detects stuck tasks, missing dependencies, status inconsistencies, and integration issues.

use super::Checker;
use crate::types::{DiagnosticCategory, DiagnosticFinding, Severity};
use chrono::{DateTime, Utc};
use std::path::Path;

/// Task stuck threshold in hours.
const STUCK_THRESHOLD_HOURS: i64 = 24;

/// Checker for task issues.
pub struct TaskChecker {
    runtime_dir: std::path::PathBuf,
}

impl TaskChecker {
    pub fn new(runtime_dir: &Path) -> Self {
        Self {
            runtime_dir: runtime_dir.to_path_buf(),
        }
    }

    fn load_task_files(&self) -> Result<Vec<(String, serde_json::Value)>, anyhow::Error> {
        let mut tasks = Vec::new();
        let tasks_dir = self.runtime_dir.join("tasks");

        if !tasks_dir.exists() {
            return Ok(tasks);
        }

        for entry in std::fs::read_dir(&tasks_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue; // Skip temp files
            }

            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(id) = json.get("task_id").and_then(|v| v.as_str()) {
                        tasks.push((id.to_string(), json));
                    }
                }
            }
        }

        Ok(tasks)
    }

    fn check_stuck_tasks(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let tasks = self.load_task_files()?;

        let now = Utc::now();

        for (task_id, json) in &tasks {
            let status = json
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            // Check tasks that should have progressed
            if matches!(
                status,
                "in_progress" | "InProgress" | "assigned" | "Assigned" | "blocked" | "Blocked"
            ) {
                if let Some(updated) = json
                    .get("updated_at")
                    .and_then(|v| v.as_str())
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&Utc))
                {
                    let elapsed = now.signed_duration_since(updated);
                    if elapsed.num_hours() > STUCK_THRESHOLD_HOURS {
                        findings.push(
                            DiagnosticFinding::new(
                                DiagnosticCategory::Task,
                                Severity::Warning,
                                format!(
                                    "Stuck task: {} (status: {}, {}h since update)",
                                    task_id,
                                    status,
                                    elapsed.num_hours()
                                ),
                            )
                            .with_subject(task_id.clone())
                            .with_description(format!(
                                "Task in '{}' state for over {} hours",
                                status,
                                elapsed.num_hours()
                            ))
                            .with_suggestion("Check if worker is still active or reassign task"),
                        );
                    }
                }
            }
        }

        Ok(findings)
    }

    fn check_missing_dependencies(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let tasks = self.load_task_files()?;

        let task_ids: std::collections::HashSet<&str> =
            tasks.iter().map(|(id, _)| id.as_str()).collect();

        for (task_id, json) in &tasks {
            if let Some(deps) = json.get("dependencies").and_then(|v| v.as_array()) {
                for dep in deps {
                    if let Some(dep_id) = dep.as_str() {
                        if !task_ids.contains(dep_id) {
                            findings.push(
                                DiagnosticFinding::new(
                                    DiagnosticCategory::Task,
                                    Severity::Error,
                                    format!("Missing dependency: {} → {}", task_id, dep_id),
                                )
                                .with_subject(task_id.clone())
                                .with_description(format!(
                                    "Task depends on non-existent task {}",
                                    dep_id
                                ))
                                .with_suggestion(
                                    "Remove dependency reference or create missing task",
                                ),
                            );
                        }
                    }
                }
            }
        }

        Ok(findings)
    }

    fn check_integration_status_mismatch(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let tasks = self.load_task_files()?;
        let now = Utc::now();

        for (task_id, json) in &tasks {
            let status = json
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            let integration_status = json.get("integration_status").and_then(|v| v.as_str());

            let parent_id = json.get("parent_id").and_then(|v| v.as_str());

            let task_type = json
                .get("task_type")
                .and_then(|v| v.as_str())
                .unwrap_or("task");

            // Check for subtasks with mismatched integration status
            if parent_id.is_some() && task_type == "task" {
                // Subtask should have integration_status if parent is an epic
                if let ("Approved" | "approved", None | Some("pending")) =
                    (status, integration_status)
                {
                    // Approved subtask without integration_status might be stuck
                    let updated = json
                        .get("updated_at")
                        .and_then(|v| v.as_str())
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.with_timezone(&Utc));

                    if let Some(updated) = updated {
                        let elapsed = now.signed_duration_since(updated);
                        // If approved for over 1 hour without integration, flag it
                        if elapsed.num_minutes() > 60 {
                            findings.push(
                                DiagnosticFinding::new(
                                    DiagnosticCategory::Task,
                                    Severity::Warning,
                                    format!("Approved subtask not integrated: {}", task_id),
                                )
                                .with_subject(task_id.clone())
                                .with_description("Subtask approved but integration_status not set to 'integrated'")
                                .with_suggestion("Verify subtask branch was merged into epic branch and update status")
                            );
                        }
                    }
                }
            }

            // Check epic tasks
            if task_type == "epic" {
                // Epic with integrated children but integration_status not reflecting
                if let Some("integrated") = integration_status {
                    // Valid: epic marked as integrated
                } else if status == "closed" || status == "Closed" {
                    // Valid: epic closed without integration
                }
            }
        }

        Ok(findings)
    }

    fn check_epic_subtask_status(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let tasks = self.load_task_files()?;

        // Build map of parent_id -> children
        let mut epic_children: std::collections::HashMap<String, Vec<&str>> =
            std::collections::HashMap::new();
        for (task_id, json) in &tasks {
            if let Some(parent_id) = json.get("parent_id").and_then(|v| v.as_str()) {
                epic_children
                    .entry(parent_id.to_string())
                    .or_default()
                    .push(task_id.as_str());
            }
        }

        // Check epics for issues with subtasks
        for (task_id, json) in &tasks {
            let task_type = json
                .get("task_type")
                .and_then(|v| v.as_str())
                .unwrap_or("task");

            if task_type == "epic" {
                let children = epic_children
                    .get(task_id)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);

                if children.is_empty() {
                    // Epic with no children could be valid or issue
                    let status = json
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");

                    if status == "in_progress" || status == "InProgress" {
                        findings.push(
                            DiagnosticFinding::new(
                                DiagnosticCategory::Task,
                                Severity::Info,
                                format!("Epic has no subtasks: {}", task_id),
                            )
                            .with_subject(task_id.clone())
                            .with_description("Epic in progress but has no child tasks")
                            .with_suggestion("Consider creating subtasks or closing epic"),
                        );
                    }
                } else {
                    // Check if all children are complete
                    let mut all_approved = true;
                    let mut has_integrated = false;

                    for child_id in children {
                        if let Some((_, child_json)) = tasks.iter().find(|(id, _)| id == child_id) {
                            let child_status = child_json
                                .get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            let child_integ = child_json
                                .get("integration_status")
                                .and_then(|v| v.as_str());

                            if child_status != "approved" && child_status != "Approved" {
                                all_approved = false;
                            }
                            if child_integ == Some("integrated") {
                                has_integrated = true;
                            }
                        }
                    }

                    // If all children approved, epic should be ready to merge
                    if all_approved && !children.is_empty() {
                        let epic_status = json
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");

                        let integration_status =
                            json.get("integration_status").and_then(|v| v.as_str());

                        if epic_status != "approved"
                            && epic_status != "Approved"
                            && epic_status != "merged"
                            && epic_status != "Merged"
                            && epic_status != "closed"
                            && epic_status != "Closed"
                        {
                            // Epic not marked approved despite all children approved
                            findings.push(
                                DiagnosticFinding::new(
                                    DiagnosticCategory::Task,
                                    Severity::Info,
                                    format!("Epic pending approval: {}", task_id),
                                )
                                .with_subject(task_id.clone())
                                .with_description("All subtasks approved but epic not yet approved")
                                .with_suggestion(
                                    "Update epic status to approved and proceed to merge",
                                ),
                            );
                        }

                        // Epic should have integration_status if subtasks integrated
                        if has_integrated && integration_status.is_none() {
                            findings.push(
                                DiagnosticFinding::new(
                                    DiagnosticCategory::Task,
                                    Severity::Warning,
                                    format!("Epic missing integration_status: {}", task_id),
                                )
                                .with_subject(task_id.clone())
                                .with_description(
                                    "Subtasks integrated but epic integration_status not set",
                                )
                                .with_suggestion("Set epic integration_status to 'integrated'"),
                            );
                        }
                    }
                }
            }
        }

        Ok(findings)
    }
}

impl Checker for TaskChecker {
    fn category(&self) -> DiagnosticCategory {
        DiagnosticCategory::Task
    }

    fn check(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        findings.extend(self.check_stuck_tasks()?);
        findings.extend(self.check_missing_dependencies()?);
        findings.extend(self.check_integration_status_mismatch()?);
        findings.extend(self.check_epic_subtask_status()?);
        Ok(findings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_checker() {
        let checker = TaskChecker::new(Path::new("/tmp"));
        assert_eq!(checker.category(), DiagnosticCategory::Task);
    }
}

//! Review diagnostic checker.
//!
//! Detects stale reviews, missing approvals, and review consistency issues.

use super::Checker;
use crate::types::{DiagnosticCategory, DiagnosticFinding, Severity};
use chrono::{DateTime, Utc};
use std::path::Path;

/// Review stuck threshold in hours.
const REVIEW_STUCK_THRESHOLD_HOURS: i64 = 48;

/// Checker for review issues.
pub struct ReviewChecker {
    runtime_dir: std::path::PathBuf,
}

impl ReviewChecker {
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
                continue;
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

    fn load_review_states(
        &self,
    ) -> Result<Vec<(String, String, serde_json::Value)>, anyhow::Error> {
        let mut reviews = Vec::new();
        let reviews_dir = self.runtime_dir.join("reviews");

        if !reviews_dir.exists() {
            return Ok(reviews);
        }

        // Iterate over task directories
        for task_dir_entry in std::fs::read_dir(&reviews_dir)? {
            let task_dir_entry = task_dir_entry?;
            let task_id = task_dir_entry.file_name().to_string_lossy().to_string();
            let task_review_dir = task_dir_entry.path();

            if !task_review_dir.is_dir() {
                continue;
            }

            // Check state.json in each task review directory
            let state_file = task_review_dir.join("state.json");
            if state_file.exists() {
                if let Ok(content) = std::fs::read_to_string(&state_file) {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(review_id) =
                            json.get("current_review_id").and_then(|v| v.as_str())
                        {
                            reviews.push((task_id.clone(), review_id.to_string(), json));
                        }
                    }
                }
            }
        }

        Ok(reviews)
    }

    fn check_stale_reviews(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let reviews = self.load_review_states()?;
        let now = Utc::now();

        for (task_id, review_id, json) in &reviews {
            let status = json
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            // Reviews in "collecting" or "pending" state for too long
            if matches!(status, "collecting" | "pending") {
                if let Some(updated) = json
                    .get("updated_at")
                    .and_then(|v| v.as_str())
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&Utc))
                {
                    let elapsed = now.signed_duration_since(updated);
                    if elapsed.num_hours() > REVIEW_STUCK_THRESHOLD_HOURS {
                        findings.push(
                            DiagnosticFinding::new(
                                DiagnosticCategory::Review,
                                Severity::Warning,
                                format!("Stale review: {} for task {}", review_id, task_id),
                            )
                            .with_subject(format!("{} ({})", review_id, task_id))
                            .with_description(format!(
                                "Review in '{}' state for over {} hours",
                                status,
                                elapsed.num_hours()
                            ))
                            .with_suggestion(
                                "Check if reviewers are still active or cancel review",
                            ),
                        );
                    }
                }
            }
        }

        Ok(findings)
    }

    fn check_missing_review_completions(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let tasks = self.load_task_files();
        let reviews = self.load_review_states()?;

        let tasks = tasks?;

        // Build set of tasks in review
        let review_task_ids: std::collections::HashSet<&str> = reviews
            .iter()
            .map(|(task_id, _, _)| task_id.as_str())
            .collect();

        for (task_id, json) in &tasks {
            let status = json
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            // Task in review should have a review state
            if (status == "in_review" || status == "InReview")
                && !review_task_ids.contains(task_id.as_str())
            {
                findings.push(
                    DiagnosticFinding::new(
                        DiagnosticCategory::Review,
                        Severity::Error,
                        format!("In-review task missing review state: {}", task_id),
                    )
                    .with_subject(task_id.clone())
                    .with_description("Task marked as 'in_review' but has no active review state")
                    .with_suggestion("Create review state file or update task status"),
                );
            }
        }

        Ok(findings)
    }

    fn check_score_mismatches(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        // Would check individual review score files vs aggregated state
        // This requires loading individual reviewer submission files

        let reviews_dir = self.runtime_dir.join("reviews");
        if !reviews_dir.exists() {
            return Ok(findings);
        }

        for task_dir_entry in std::fs::read_dir(&reviews_dir)? {
            let task_dir_entry = task_dir_entry?;
            let task_id = task_dir_entry.file_name().to_string_lossy().to_string();
            let task_review_dir = task_dir_entry.path();

            if !task_review_dir.is_dir() {
                continue;
            }

            // Count submissions vs expected panel size
            let submissions_dir = task_review_dir.join("submissions");
            if submissions_dir.exists() {
                let submission_count = std::fs::read_dir(&submissions_dir)
                    .map(|entries| entries.flatten().count())
                    .unwrap_or(0);

                // Check state for panel size expectation
                let state_file = task_review_dir.join("state.json");
                if let Ok(content) = std::fs::read_to_string(&state_file) {
                    if let Ok(state) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(panel_size) = state.get("panel_size").and_then(|v| v.as_u64()) {
                            let status = state
                                .get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");

                            // If collecting and all panel members have submitted, should complete
                            if status == "collecting" && submission_count >= panel_size as usize {
                                findings.push(
                                    DiagnosticFinding::new(
                                        DiagnosticCategory::Review,
                                        Severity::Warning,
                                        format!("Review collection complete but not processed: {}", task_id),
                                    )
                                    .with_subject(task_id.clone())
                                    .with_description(format!(
                                        "All {} panel members have submitted but status is still 'collecting'",
                                        submission_count
                                    ))
                                    .with_suggestion("Submit consolidated review to move task forward")
                                );
                            }
                        }
                    }
                }
            }
        }

        Ok(findings)
    }

    fn check_inconsistent_states(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let reviews = self.load_review_states()?;

        for (task_id, review_id, json) in &reviews {
            let status = json
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            let current_review_id = json
                .get("current_review_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            // Check for empty/invalid review IDs
            if (status == "collecting" || status == "pending") && current_review_id.is_empty() {
                findings.push(
                    DiagnosticFinding::new(
                        DiagnosticCategory::Review,
                        Severity::Error,
                        format!("Review state missing current_review_id: {}", task_id),
                    )
                    .with_subject(task_id.clone())
                    .with_description("Review in active state but no current_review_id set")
                    .with_suggestion("Update review state with valid review_id"),
                );
            }

            // Check for old format review IDs (T- prefixed)
            if current_review_id.starts_with("T-") {
                findings.push(
                    DiagnosticFinding::new(
                        DiagnosticCategory::Review,
                        Severity::Info,
                        format!("Legacy review ID format: {}", review_id),
                    )
                    .with_subject(review_id.clone())
                    .with_suggestion("Consider migrating to R- prefixed review IDs"),
                );
            }
        }

        Ok(findings)
    }
}

impl Checker for ReviewChecker {
    fn category(&self) -> DiagnosticCategory {
        DiagnosticCategory::Review
    }

    fn check(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        findings.extend(self.check_stale_reviews()?);
        findings.extend(self.check_missing_review_completions()?);
        findings.extend(self.check_score_mismatches()?);
        findings.extend(self.check_inconsistent_states()?);
        Ok(findings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_checker() {
        let checker = ReviewChecker::new(Path::new("/tmp"));
        assert_eq!(checker.category(), DiagnosticCategory::Review);
    }
}

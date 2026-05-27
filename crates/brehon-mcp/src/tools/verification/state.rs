use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use brehon_types::ReviewVerdict;

use super::helpers::reviews_dir;
use crate::tools::assignment_observability::AssignmentPropagation;
use crate::tools::stability::refresh_runtime_stability_counters;

// ── Storage wrapper types ────────────────────────────────────────────────────
//
// These are the on-disk JSON shapes. They are NOT the brehon-types or
// brehon-review structs — explicit mapping functions bridge the boundary.

/// Task-scoped review state persisted in `.brehon/runtime/reviews/{task_id}/state.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewState {
    pub task_id: String,
    pub status: String, // "collecting", "approved", "changes_requested", "rejected", "escalated"
    pub current_round: u32,
    #[serde(default = "default_cycle_start_round")]
    pub cycle_start_round: u32,
    #[serde(default = "default_review_epoch_start_round")]
    pub review_epoch_start_round: u32,
    pub current_review_id: String,
    pub max_rounds: u8,
    #[serde(default = "default_panel_id")]
    pub panel_id: String,
    #[serde(default = "default_panel_mode")]
    pub panel_mode: String, // "full_council" | "fixed_size"
    pub panel: Vec<String>,
    pub submissions_received: Vec<String>,
    #[serde(default)]
    pub(crate) reviewer_assignments: BTreeMap<String, AssignmentPropagation>,
    pub created_at: String,
    pub updated_at: String,
}

pub(super) fn default_panel_id() -> String {
    super::panel::IMPLICIT_PANEL_ID.to_string()
}

pub(super) fn default_panel_mode() -> String {
    "full_council".to_string()
}

pub(super) fn default_cycle_start_round() -> u32 {
    1
}

pub(super) fn default_review_epoch_start_round() -> u32 {
    1
}

pub(crate) fn review_cycle_round(state: &ReviewState, absolute_round: u32) -> u32 {
    let cycle_start = state.cycle_start_round.max(1);
    if absolute_round >= cycle_start {
        absolute_round - cycle_start + 1
    } else {
        absolute_round
    }
}

pub(crate) fn current_review_cycle_round(state: &ReviewState) -> u32 {
    review_cycle_round(state, state.current_round)
}

pub(crate) const MAX_REVIEW_RESET_CYCLES_PER_TASK: u32 = 3;

pub(crate) fn total_review_round_limit(max_rounds: u8) -> u32 {
    u32::from(max_rounds.max(1)) * MAX_REVIEW_RESET_CYCLES_PER_TASK
}

pub(crate) fn review_epoch_round(state: &ReviewState, absolute_round: u32) -> u32 {
    let epoch_start = state.review_epoch_start_round.max(1);
    if absolute_round >= epoch_start {
        absolute_round - epoch_start + 1
    } else {
        absolute_round
    }
}

pub(crate) fn current_review_epoch_round(state: &ReviewState) -> u32 {
    review_epoch_round(state, state.current_round)
}

pub(crate) fn total_review_rounds_exhausted(state: &ReviewState) -> bool {
    current_review_epoch_round(state) >= total_review_round_limit(state.max_rounds)
}

pub(crate) fn next_review_round_would_exceed_total_limit(
    state: &ReviewState,
    next_round: u32,
) -> bool {
    review_epoch_round(state, next_round) > total_review_round_limit(state.max_rounds)
}

/// Per-round request metadata in `.brehon/runtime/reviews/{task_id}/round-N/request.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRequestFile {
    pub task_id: String,
    pub review_id: String,
    pub requested_by: String,
    pub requested_at: String,
    pub title: String,
    pub description: String,
    pub commit: String,
    #[serde(default)]
    pub base_commit: String,
    #[serde(default)]
    pub merge_target_head: String,
    #[serde(default)]
    pub commits: Vec<String>,
    #[serde(default)]
    pub resolved_empty_commit_set: bool,
    #[serde(default)]
    pub review_fingerprint: Value,
    #[serde(default)]
    pub reviewer_prompts: BTreeMap<String, String>,
    pub context: String,
}

pub(crate) fn reviewed_commits(request: &ReviewRequestFile) -> Vec<String> {
    if request.resolved_empty_commit_set {
        Vec::new()
    } else if !request.commits.is_empty() {
        request.commits.clone()
    } else if !request.commit.trim().is_empty() {
        vec![request.commit.trim().to_string()]
    } else {
        Vec::new()
    }
}

/// Individual reviewer submission in `.brehon/runtime/reviews/{task_id}/round-N/{reviewer}.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSubmission {
    pub review_id: String,
    pub reviewer: String,
    pub round: u32,
    pub score: u8,
    pub verdict: String, // "approved", "needs_revision", "rejected"
    pub summary: String,
    pub findings: Vec<StoredFinding>,
    pub submitted_at: String,
}

/// Finding wrapper for on-disk storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredFinding {
    pub description: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub severity: String, // "blocking", "suggestion", "nitpick"
    pub suggestion: Option<String>,
}

/// Serializable wrapper for per-reviewer calibration statistics.
/// Bridges `brehon_review::calibration::PerReviewerStats` to JSON persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCalibrationEntry {
    pub reviewer_id: String,
    pub review_count: u32,
    pub average_score: Option<f64>,
    pub std_deviation: Option<f64>,
    pub approval_rate: Option<f64>,
    pub approval_count: u32,
    pub rejection_count: u32,
    pub changes_requested_count: u32,
    pub is_outlier: bool,
}

/// Full calibration snapshot persisted in `.brehon/runtime/reviews/calibration.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCalibration {
    pub reviewers: Vec<StoredCalibrationEntry>,
    pub global_average: Option<f64>,
    pub global_std_deviation: Option<f64>,
    pub global_approval_rate: Option<f64>,
    pub outlier_threshold: f64,
    pub updated_at: String,
}

/// Consolidated report in `.brehon/runtime/reviews/{task_id}/round-N/consolidated.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidatedReport {
    pub review_id: String,
    pub task_id: String,
    pub round: u32,
    pub outcome: String,
    pub scores: Value,
    pub average_score: f64,
    pub min_score: u8,
    pub approval_count: usize,
    pub threshold_result: String,
    pub threshold_reason: String,
    pub blocking: Vec<StoredFinding>,
    pub suggestions: Vec<StoredFinding>,
    pub nitpicks: Vec<StoredFinding>,
    pub dissent: Vec<String>,
    pub evaluated_at: String,
}

// ── Domain mapping ───────────────────────────────────────────────────────────

impl StoredFinding {
    pub(crate) fn to_review_finding(&self) -> brehon_types::ReviewFinding {
        let severity = match self.severity.as_str() {
            "blocking" => brehon_types::CommentSeverity::Blocking,
            "suggestion" => brehon_types::CommentSeverity::Suggestion,
            "nitpick" => brehon_types::CommentSeverity::Nitpick,
            _ => brehon_types::CommentSeverity::Suggestion,
        };
        let location = match (&self.file, self.line) {
            (Some(file), Some(line)) => Some(brehon_types::InlineComment {
                file: file.clone(),
                line,
                content: self.description.clone(),
                severity,
            }),
            _ => None,
        };
        brehon_types::ReviewFinding {
            description: self.description.clone(),
            location,
            suggestion: self.suggestion.clone(),
            severity,
        }
    }

    pub(crate) fn from_review_finding(f: &brehon_types::ReviewFinding) -> Self {
        let severity = match f.severity {
            brehon_types::CommentSeverity::Blocking => "blocking",
            brehon_types::CommentSeverity::Suggestion => "suggestion",
            brehon_types::CommentSeverity::Nitpick => "nitpick",
        };
        let (file, line) = match &f.location {
            Some(loc) => (Some(loc.file.clone()), Some(loc.line)),
            None => (None, None),
        };
        Self {
            description: f.description.clone(),
            file,
            line,
            severity: severity.to_string(),
            suggestion: f.suggestion.clone(),
        }
    }
}

pub(crate) fn parse_verdict(s: &str) -> ReviewVerdict {
    match s {
        "approved" => ReviewVerdict::Approve,
        "needs_revision" | "changes_requested" => ReviewVerdict::ChangesRequested,
        "rejected" => ReviewVerdict::Reject,
        _ => ReviewVerdict::ChangesRequested,
    }
}

/// Map domain verdict back to string for storage/display.
pub fn verdict_str(v: &ReviewVerdict) -> &'static str {
    match v {
        ReviewVerdict::Approve => "approved",
        ReviewVerdict::ChangesRequested => "changes_requested",
        ReviewVerdict::Reject => "rejected",
    }
}

// ── File I/O helpers ─────────────────────────────────────────────────────────

pub(crate) fn task_review_dir(task_id: &str) -> Option<PathBuf> {
    reviews_dir().map(|d| d.join(task_id))
}

pub(crate) fn round_dir(task_id: &str, round: u32) -> Option<PathBuf> {
    task_review_dir(task_id).map(|d| d.join(format!("round-{round}")))
}

pub(crate) fn highest_round_on_disk(task_id: &str) -> u32 {
    let Some(dir) = task_review_dir(task_id) else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };

    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .strip_prefix("round-")
                .and_then(|suffix| suffix.parse::<u32>().ok())
        })
        .max()
        .unwrap_or(0)
}

const REVIEW_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const REVIEW_LOCK_RETRY: Duration = Duration::from_millis(10);
const REVIEW_LOCK_STALE_AFTER: Duration = Duration::from_secs(30);

pub(super) struct ReviewStateLock {
    path: PathBuf,
}

impl Drop for ReviewStateLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(crate) fn clear_stale_review_lock(path: &std::path::Path) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    let Ok(modified) = metadata.modified() else {
        return;
    };
    let Ok(age) = modified.elapsed() else {
        return;
    };
    if age >= REVIEW_LOCK_STALE_AFTER {
        let _ = std::fs::remove_file(path);
    }
}

pub(super) async fn acquire_review_lock(task_id: &str) -> std::io::Result<ReviewStateLock> {
    let dir = task_review_dir(task_id)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "No reviews dir"))?;
    std::fs::create_dir_all(&dir)?;

    let path = dir.join(".state.lock");
    let start = Instant::now();

    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(ReviewStateLock { path }),
            Err(err)
                if err.kind() == ErrorKind::AlreadyExists
                    && start.elapsed() < REVIEW_LOCK_TIMEOUT =>
            {
                clear_stale_review_lock(&path);
                tokio::time::sleep(REVIEW_LOCK_RETRY).await;
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                return Err(std::io::Error::new(
                    ErrorKind::TimedOut,
                    format!("Timed out waiting for review state lock for task {task_id}"),
                ));
            }
            Err(err) => return Err(err),
        }
    }
}

pub(crate) fn read_review_state(task_id: &str) -> Option<ReviewState> {
    let path = task_review_dir(task_id)?.join("state.json");
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

pub(crate) fn delete_review_state(task_id: &str) -> std::io::Result<bool> {
    let Some(dir) = task_review_dir(task_id) else {
        return Ok(false);
    };
    let path = dir.join("state.json");
    match std::fs::remove_file(&path) {
        Ok(()) => {
            refresh_runtime_stability_counters();
            Ok(true)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

pub(crate) fn clear_obsolete_review_state_for_resumed_work(task_id: &str) -> std::io::Result<bool> {
    let Some(state) = read_review_state(task_id) else {
        return Ok(false);
    };
    if matches!(
        state.status.as_str(),
        "changes_requested" | "released" | "rejected" | "escalated"
    ) {
        return delete_review_state(task_id);
    }
    Ok(false)
}

pub(crate) fn write_review_state(task_id: &str, state: &ReviewState) -> std::io::Result<()> {
    let dir = task_review_dir(task_id)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "No reviews dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("state.json");
    let tmp = path.with_extension("tmp");
    let json = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)?;
    refresh_runtime_stability_counters();
    Ok(())
}

pub(crate) fn write_round_request(
    task_id: &str,
    round: u32,
    req: &ReviewRequestFile,
) -> std::io::Result<()> {
    let dir = round_dir(task_id, round)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "No round dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("request.json");
    let json = serde_json::to_string_pretty(req).map_err(std::io::Error::other)?;
    std::fs::write(&path, &json)
}

pub(crate) fn write_submission(
    task_id: &str,
    round: u32,
    reviewer: &str,
    sub: &StoredSubmission,
) -> std::io::Result<()> {
    let dir = round_dir(task_id, round)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "No round dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{reviewer}.json"));
    let tmp = path.with_extension("tmp");
    let json = serde_json::to_string_pretty(sub).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)
}

pub(crate) fn delete_submission(task_id: &str, round: u32, reviewer: &str) -> std::io::Result<()> {
    let dir = round_dir(task_id, round)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "No round dir"))?;
    let path = dir.join(format!("{reviewer}.json"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Roll back a persisted submission and its in-memory state entry.
/// Deletes the on-disk submission file, removes the reviewer from the
/// `submissions_received` list, drops their assignment propagation record, and
/// re-persists the updated review state.
///
/// This is a best-effort multi-step cleanup, not a transactional guarantee.
/// If the process crashes between `delete_submission` and `write_review_state`,
/// the on-disk state may be inconsistent until the next operation.
///
/// Returns `Ok(())` when both the file deletion and state re-write succeed.
/// Returns `Err` with a description if either step fails so callers can surface
/// a distinct rollback-failed warning alongside the original error.
pub(crate) fn rollback_review_submission(
    task_id: &str,
    round: u32,
    reviewer: &str,
    state: &mut ReviewState,
) -> Result<(), String> {
    let mut errors = Vec::new();

    if let Err(err) = delete_submission(task_id, round, reviewer) {
        errors.push(format!("failed to delete submission file: {err}"));
    }
    state.submissions_received.retain(|r| r != reviewer);
    state.reviewer_assignments.remove(reviewer);

    if let Err(err) = write_review_state(task_id, state) {
        errors.push(format!("failed to rewrite review state: {err}"));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Rollback for reviewer {reviewer} on task {task_id} round {round} partially failed: {}",
            errors.join("; ")
        ))
    }
}

pub(crate) fn read_round_request(task_id: &str, round: u32) -> Option<ReviewRequestFile> {
    let dir = round_dir(task_id, round)?;
    let path = dir.join("request.json");
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

pub(crate) fn read_round_submissions(task_id: &str, round: u32) -> Vec<StoredSubmission> {
    let Some(dir) = round_dir(task_id, round) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let expected_review_id = read_round_request(task_id, round).map(|request| request.review_id);

    entries
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            e.path().extension().is_some_and(|ext| ext == "json")
                && name != "request.json"
                && name != "consolidated.json"
                && !name.starts_with('.')
        })
        .filter_map(|e| {
            let content = std::fs::read_to_string(e.path()).ok()?;
            serde_json::from_str::<StoredSubmission>(&content).ok()
        })
        .filter(|submission| {
            expected_review_id
                .as_ref()
                .map(|review_id| submission.review_id == *review_id)
                .unwrap_or(true)
        })
        .collect()
}

pub(crate) fn find_review_request_by_id(review_id: &str) -> Option<(String, u32)> {
    let reviews_root = reviews_dir()?;
    let task_entries = std::fs::read_dir(&reviews_root).ok()?;

    for task_entry in task_entries.flatten() {
        if !task_entry.path().is_dir() {
            continue;
        }

        let task_id = task_entry.file_name().to_string_lossy().to_string();
        let Ok(round_entries) = std::fs::read_dir(task_entry.path()) else {
            continue;
        };

        for round_entry in round_entries.flatten() {
            if !round_entry.path().is_dir() {
                continue;
            }

            let round_name = round_entry.file_name().to_string_lossy().to_string();
            let Some(round_str) = round_name.strip_prefix("round-") else {
                continue;
            };
            let Ok(round) = round_str.parse::<u32>() else {
                continue;
            };

            if read_round_request(&task_id, round)
                .is_some_and(|request| request.review_id == review_id)
            {
                return Some((task_id, round));
            }
        }
    }

    None
}

pub(crate) fn write_consolidated(
    task_id: &str,
    round: u32,
    report: &ConsolidatedReport,
) -> std::io::Result<()> {
    let dir = round_dir(task_id, round)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "No round dir"))?;
    let path = dir.join("consolidated.json");
    let json = serde_json::to_string_pretty(report).map_err(std::io::Error::other)?;
    std::fs::write(&path, &json)
}

/// Best-effort delete of a consolidated report for a round.
///
/// Returns `Ok(())` when the file is removed or already absent.
/// Returns `Err` only for unexpected I/O failures.
pub(crate) fn delete_consolidated(task_id: &str, round: u32) -> std::io::Result<()> {
    let dir = round_dir(task_id, round)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "No round dir"))?;
    let path = dir.join("consolidated.json");
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

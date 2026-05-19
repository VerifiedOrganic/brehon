//! Prompt retry / dead letter, review staleness, worker recovery, and task context helpers.
//!
//! Several helpers here (`attempt_auto_recover_stalled_worker`,
//! `inspect_worker_worktree_state`, `escalate_worker_unmerged_conflict`,
//! prompt-queue sweep helpers, and their supporting fns) are staged
//! infrastructure: unit-tested but not yet wired into the production event
//! loop. The module-level `allow(dead_code)` below silences the noise until
//! callers are connected; new additions should still be wired up.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use brehon_types::task::{
    infer_task_completion_mode, normalize_task_status, parse_task_completion_mode, Priority, Task,
    TaskCompletionMode, TaskId, TaskStatus,
};

use brehon_mux::{
    Mux, PaneKind, SessionScopedQueue, StoredScopedEntry, TaskBlockedReason, TaskContextDetails,
    TaskContextSnapshot,
};
use serde::{Deserialize, Serialize};

use super::types::*;

const LEGACY_RUNTIME_SESSION_NAME: &str = "_legacy";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DeadLetterEntry {
    pub original_path: String,
    pub target: String,
    pub from: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    pub error: String,
    pub reason: String,
    pub dead_lettered_at: String,
}

pub(crate) fn push_dashboard_event(
    dashboard_data: &Arc<std::sync::Mutex<DashboardData>>,
    description: impl Into<String>,
) {
    let mut dashboard = dashboard_data.lock().unwrap();
    dashboard.events.push(EventInfo {
        timestamp: chrono::Local::now().format("%M:%S").to_string(),
        description: description.into(),
    });
    const MAX_EVENTS: usize = 50;
    if dashboard.events.len() > MAX_EVENTS {
        let drop_count = dashboard.events.len() - MAX_EVENTS;
        dashboard.events.drain(0..drop_count);
    }
}

/// Decide whether a queued prompt should be delivered in the current TUI
/// session.
///
/// Rules:
/// - If the TUI has no session name (rare; no runtime scoping active), accept
///   any prompt.
/// - If the prompt payload has no `session_name` — i.e. the writer lost
///   `BREHON_SESSION_NAME` propagation and couldn't resolve a fallback either
///   — treat it as an **orphan** and deliver it. The writer's fallback chain
///   (env → current-session.json → `_legacy/`) only reaches this state as a
///   last resort, and dropping those prompts silently is strictly worse than
///   risking cross-session delivery in multi-session setups (which aren't
///   supported today).
/// - Otherwise, enforce strict equality: prompts tagged for another session
///   must not be delivered here.
pub(crate) fn queued_prompt_matches_session(
    expected_session: Option<&str>,
    prompt_session_name: Option<&str>,
) -> bool {
    let expected_session = expected_session
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(expected_session) = expected_session else {
        return true;
    };
    let prompt_session_name = prompt_session_name
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match prompt_session_name {
        Some(value) => value == expected_session,
        None => true, // orphan — deliver
    }
}

/// Decoded queued prompt payload ready for delivery.
pub(crate) struct QueuedPromptPayload {
    pub(crate) target: String,
    pub(crate) from: Option<String>,
    pub(crate) message: String,
    pub(crate) session_name: Option<String>,
    pub(crate) prompt_id: Option<String>,
}

pub(crate) fn read_queued_prompt(path: &Path) -> Option<QueuedPromptPayload> {
    let content = std::fs::read_to_string(path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;

    let (session_source, prompt_source) = if let Some(entry) = parsed.get("entry") {
        (parsed.get("session_name"), entry)
    } else {
        (parsed.get("session_name"), &parsed)
    };

    let target = prompt_source.get("target")?.as_str()?.to_string();
    let from = prompt_source
        .get("from")
        .and_then(|v| v.as_str())
        .map(|value| value.to_string());
    let session_name = session_source
        .and_then(|v| v.as_str())
        .map(|value| value.to_string());
    let prompt_id = prompt_source
        .get("prompt_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let message = prompt_source.get("message")?.as_str()?.to_string();
    Some(QueuedPromptPayload {
        target,
        from,
        message,
        session_name,
        prompt_id,
    })
}

pub(crate) fn prompt_retry_meta_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "queued.prompt".to_string());
    path.with_file_name(format!("{file_name}.retry.json"))
}

pub(crate) fn runtime_prompt_queue_dir_for_session(
    brehon_root: &Path,
    session_name: Option<&str>,
) -> PathBuf {
    let base = brehon_root.join("runtime").join("prompt-queue");
    match session_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(session_name) => base.join(session_name),
        None => base,
    }
}

/// The orphan/legacy bucket used as a last-resort fallback by the prompt-queue
/// writer when it cannot resolve a session name. The TUI reader drains this
/// directory on every sweep so that a broken env-propagation chain upstream
/// cannot silently lose prompts (e.g. supervisor approval notifications).
pub(crate) fn runtime_prompt_queue_legacy_dir(brehon_root: &Path) -> PathBuf {
    brehon_root
        .join("runtime")
        .join("prompt-queue")
        .join("_legacy")
}

/// All prompt-queue directories the TUI reader should sweep on each tick.
///
/// Returns the base legacy dir, the session-scoped dir, and then the `_legacy/`
/// orphan bucket. Callers iterate them in order; files that exist in multiple
/// locations won't be double-delivered because successful delivery removes the
/// source file.
pub(crate) fn runtime_prompt_queue_sweep_dirs(
    brehon_root: &Path,
    session_name: Option<&str>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(3);
    let base = brehon_root.join("runtime").join("prompt-queue");
    dirs.push(base);
    let session_dir = runtime_prompt_queue_dir_for_session(brehon_root, session_name);
    if !dirs.iter().any(|existing| existing == &session_dir) {
        dirs.push(session_dir);
    }
    let legacy = runtime_prompt_queue_legacy_dir(brehon_root);
    if !dirs.iter().any(|existing| existing == &legacy) {
        dirs.push(legacy);
    }
    dirs
}

pub(crate) fn prompt_dead_letter_queue_dir(brehon_root: &Path) -> PathBuf {
    brehon_root.join("runtime").join("prompt-dead-letter")
}

pub(crate) fn dead_letter_queue_for_session(
    session_name: Option<&str>,
    dead_letter_queue_dir: PathBuf,
) -> SessionScopedQueue<DeadLetterEntry> {
    let session_name = session_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(LEGACY_RUNTIME_SESSION_NAME);
    SessionScopedQueue::new(session_name, dead_letter_queue_dir)
}

pub(crate) fn list_dead_letters_for_tool(
    dead_letter_queue_dir: &Path,
) -> Vec<StoredScopedEntry<DeadLetterEntry>> {
    #[derive(Deserialize)]
    struct PersistedDeadLetterEntry {
        session_name: String,
        entry: DeadLetterEntry,
    }

    fn collect_entry_paths(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_entry_paths(&path, out);
                continue;
            }
            if !path.is_file() {
                continue;
            }
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'))
            {
                continue;
            }
            if path.extension().is_some_and(|ext| ext == "entry") {
                out.push(path);
            }
        }
    }

    fn entry_id_from_path(path: &Path) -> String {
        path.file_stem()
            .and_then(|value| value.to_str())
            .or_else(|| path.file_name().and_then(|value| value.to_str()))
            .unwrap_or_default()
            .to_string()
    }

    let mut paths = Vec::new();
    collect_entry_paths(dead_letter_queue_dir, &mut paths);
    paths.sort();

    let mut entries = Vec::new();
    for path in paths {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(payload) = serde_json::from_slice::<PersistedDeadLetterEntry>(&bytes) else {
            continue;
        };
        entries.push(StoredScopedEntry {
            id: entry_id_from_path(&path),
            session_name: payload.session_name,
            entry: payload.entry,
        });
    }
    entries
}

pub(crate) fn agent_health_path(brehon_root: &Path, agent_name: &str) -> PathBuf {
    let mut file_name = String::new();
    for ch in agent_name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            file_name.push(ch);
        } else {
            file_name.push('_');
        }
    }
    brehon_root
        .join("runtime")
        .join("agent-health")
        .join(format!("{file_name}.json"))
}

pub(crate) fn agent_is_quarantined_for_run(brehon_root: &Path, agent_name: &str) -> bool {
    let path = agent_health_path(brehon_root, agent_name);
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    value.get("status").and_then(|status| status.as_str()) == Some("unavailable")
}

pub(crate) fn clear_agent_health_marker(brehon_root: &Path, agent_name: &str) {
    let _ = std::fs::remove_file(agent_health_path(brehon_root, agent_name));
}

pub(crate) fn read_prompt_retry_attempts(path: &Path) -> u64 {
    let meta_path = prompt_retry_meta_path(path);
    let Ok(content) = std::fs::read_to_string(meta_path) else {
        return 0;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return 0;
    };
    value
        .get("attempts")
        .and_then(|value| value.as_u64())
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub(crate) struct PromptRetryDeferralSnapshot {
    pub(crate) first_deferred_at: chrono::DateTime<chrono::Utc>,
    pub(crate) last_deferred_at: chrono::DateTime<chrono::Utc>,
    pub(crate) deferrals: u64,
    pub(crate) reason: Option<String>,
}

pub(crate) fn read_prompt_retry_deferral_snapshot(
    path: &Path,
) -> Option<PromptRetryDeferralSnapshot> {
    let meta_path = prompt_retry_meta_path(path);
    let content = std::fs::read_to_string(meta_path).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&content).ok()?;
    let last_deferred_at = value
        .get("last_deferred_at")
        .and_then(|value| value.as_str())?;
    let last_deferred_at = chrono::DateTime::parse_from_rfc3339(last_deferred_at)
        .ok()?
        .with_timezone(&chrono::Utc);
    let first_deferred_at = value
        .get("first_deferred_at")
        .and_then(|value| value.as_str())
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&chrono::Utc))
        .unwrap_or(last_deferred_at);
    let deferrals = value
        .get("deferrals")
        .and_then(|value| value.as_u64())
        .unwrap_or(1);
    let reason = value
        .get("last_deferred_reason")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);

    Some(PromptRetryDeferralSnapshot {
        first_deferred_at,
        last_deferred_at,
        deferrals,
        reason,
    })
}

pub(crate) fn prompt_retry_not_due(path: &Path) -> bool {
    let meta_path = prompt_retry_meta_path(path);
    let Ok(content) = std::fs::read_to_string(meta_path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    let Some(next_retry_at) = value.get("next_retry_at").and_then(|value| value.as_str()) else {
        return false;
    };
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(next_retry_at) else {
        return false;
    };
    parsed.with_timezone(&chrono::Utc) > chrono::Utc::now()
}

pub(crate) fn clear_prompt_retry_meta(path: &Path) {
    let _ = std::fs::remove_file(prompt_retry_meta_path(path));
}

pub(crate) fn record_prompt_retry_deferral(
    path: &Path,
    retry_after: Duration,
    reason: &str,
) -> chrono::DateTime<chrono::Utc> {
    let attempts = read_prompt_retry_attempts(path);
    let existing = read_prompt_retry_deferral_snapshot(path);
    let now = chrono::Utc::now();
    let first_deferred_at = existing
        .as_ref()
        .map(|snapshot| snapshot.first_deferred_at)
        .unwrap_or(now);
    let deferrals = existing
        .as_ref()
        .map(|snapshot| snapshot.deferrals)
        .unwrap_or(0)
        .saturating_add(1);
    let next_retry_at = chrono::Utc::now()
        + chrono::Duration::from_std(retry_after).unwrap_or_else(|_| chrono::Duration::seconds(30));
    let payload = serde_json::json!({
        "attempts": attempts,
        "deferrals": deferrals,
        "first_deferred_at": first_deferred_at.to_rfc3339(),
        "last_deferred_reason": reason,
        "last_deferred_at": now.to_rfc3339(),
        "next_retry_at": next_retry_at.to_rfc3339(),
    });
    let _ = std::fs::write(
        prompt_retry_meta_path(path),
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string()),
    );
    next_retry_at
}

pub(crate) fn is_nonrecoverable_prompt_delivery_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    [
        "quarantined unavailable for this run",
        "exhausted your capacity",
        "quota will reset",
        "rate limit",
        "rate-limit",
        "too many requests",
        "authentication",
        "unauthorized",
        "forbidden",
        "invalid api key",
        "api key",
        "billing",
        "credit",
        "model not found",
        "not enabled",
        "access denied",
        "insufficient permissions",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub(crate) fn should_dead_letter_prompt_after_failure(prompt_text: &str, error: &str) -> bool {
    extract_review_timeout_identity(prompt_text).is_some()
        || is_nonrecoverable_prompt_delivery_error(error)
}

pub(crate) fn next_prompt_retry_delay(attempts: u64) -> Duration {
    match attempts {
        0 | 1 => Duration::from_secs(30),
        2 => Duration::from_secs(120),
        3 => Duration::from_secs(300),
        4 => Duration::from_secs(900),
        _ => Duration::from_secs(3600),
    }
}

pub(crate) fn queued_prompt_retry_delay(ahead_of: usize) -> Duration {
    let multiplier = u32::try_from(ahead_of)
        .unwrap_or(u32::MAX)
        .saturating_add(1);
    Duration::from_secs(2)
        .checked_mul(multiplier)
        .unwrap_or(Duration::from_secs(30))
}

pub(crate) fn record_prompt_retry_failure(
    path: &Path,
    error: &str,
) -> (u64, chrono::DateTime<chrono::Utc>) {
    let attempts = read_prompt_retry_attempts(path).saturating_add(1);
    let next_retry_at = chrono::Utc::now()
        + chrono::Duration::from_std(next_prompt_retry_delay(attempts))
            .unwrap_or_else(|_| chrono::Duration::seconds(30));
    let payload = serde_json::json!({
        "attempts": attempts,
        "last_error": error,
        "last_failed_at": chrono::Utc::now().to_rfc3339(),
        "next_retry_at": next_retry_at.to_rfc3339(),
    });
    let _ = std::fs::write(
        prompt_retry_meta_path(path),
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string()),
    );
    (attempts, next_retry_at)
}

pub(crate) fn dead_letter_prompt_for_session(
    brehon_root: &Path,
    session_name: Option<&str>,
    path: &Path,
    target: &str,
    from: Option<&str>,
    prompt_text: &str,
    error: &str,
    reason: &str,
) {
    let dead_letter_queue_dir = prompt_dead_letter_queue_dir(brehon_root);
    let dead_letter_queue =
        dead_letter_queue_for_session(session_name, dead_letter_queue_dir.clone());
    let payload = DeadLetterEntry {
        original_path: path.display().to_string(),
        target: target.to_string(),
        from: from.map(str::to_string),
        message: prompt_text.to_string(),
        prompt_id: read_queued_prompt(path).and_then(|queued| queued.prompt_id),
        error: error.to_string(),
        reason: reason.to_string(),
        dead_lettered_at: chrono::Utc::now().to_rfc3339(),
    };
    if let Err(err) = dead_letter_queue.enqueue(payload) {
        tracing::warn!(
            dead_letter_queue_dir = %dead_letter_queue_dir.display(),
            target = %target,
            error = %err,
            "failed to enqueue dead-letter prompt"
        );
    }
    let _ = std::fs::remove_file(path);
    clear_prompt_retry_meta(path);
}

pub(crate) fn extract_review_request_identity(message: &str) -> Option<(String, String)> {
    let header = message.lines().next()?.trim();
    let prefix = "Review request ";
    let suffix = " for task ";
    let rest = header.strip_prefix(prefix)?;
    let (review_id, task_and_title) = rest.split_once(suffix)?;
    let review_id = review_id.trim();
    let task_id = task_and_title
        .split_once(':')
        .map(|(task_id, _)| task_id)
        .unwrap_or(task_and_title)
        .trim();
    if review_id.is_empty() || task_id.is_empty() {
        return None;
    }
    Some((review_id.to_string(), task_id.to_string()))
}

pub(crate) fn extract_review_timeout_identity(message: &str) -> Option<(String, String)> {
    let header = message.lines().next()?.trim();
    let prefix = "Review ";
    let mid = " for task ";
    let rest = header.strip_prefix(prefix)?;
    let (review_id, task_part) = rest.split_once(mid)?;
    let (task_id, _) = task_part.split_once(" timed out and is no longer active")?;
    let task_id = task_id.trim();
    let review_id = review_id.trim();
    if review_id.is_empty() || task_id.is_empty() {
        return None;
    }
    Some((review_id.to_string(), task_id.to_string()))
}

pub(crate) fn extract_consolidated_report_identity(
    message: &str,
) -> Option<(String, String, String)> {
    let mut lines = message.lines();
    let header = lines.next()?.trim();
    let task_id = header
        .strip_prefix("Review complete for task ")?
        .trim()
        .to_string();

    let mut review_id = None;
    let mut outcome = None;
    for line in lines {
        let line = line.trim();
        if review_id.is_none() {
            review_id = line
                .strip_prefix("Review ID:")
                .map(|value| value.trim().to_string());
        }
        if outcome.is_none() {
            outcome = line
                .strip_prefix("Outcome:")
                .map(|value| value.trim().to_ascii_lowercase());
        }
        if review_id.is_some() && outcome.is_some() {
            break;
        }
    }

    Some((task_id, review_id?, outcome?))
}

pub(crate) fn rewrite_stale_consolidated_report(
    brehon_root: &Path,
    prompt_text: &str,
) -> Option<String> {
    let (task_id, review_id, outcome) = extract_consolidated_report_identity(prompt_text)?;
    let task_path = brehon_root
        .join("runtime")
        .join("tasks")
        .join(format!("{task_id}.json"));
    let task_content = std::fs::read_to_string(task_path).ok()?;
    let task: serde_json::Value = serde_json::from_str(&task_content).ok()?;
    let current_status = task.get("status").and_then(|value| value.as_str())?;
    let normalized = normalize_task_status(current_status)?;

    if !matches!(normalized, "merged" | "closed") {
        return None;
    }

    let closed_by = task
        .get("closed_by")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let closed_at = task
        .get("closed_at")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown time");

    let mut msg = format!(
        "Late consolidated review report for task {task_id}\n\
         Review ID: {review_id}\n\
         Outcome: {}\n\
         Current task status: {normalized}\n\
         Already handled by {closed_by} at {closed_at}. No action required.",
        outcome.to_uppercase()
    );

    if normalized == "merged" {
        if let Some(merged_commit) = task.get("merged_commit").and_then(|value| value.as_str()) {
            msg.push_str(&format!("\nVerified merged commit: {merged_commit}"));
        }
        if let Some(merged_branch) = task.get("merged_branch").and_then(|value| value.as_str()) {
            msg.push_str(&format!("\nMerged branch: {merged_branch}"));
        }
    }

    msg.push_str(
        "\n\nThis review report was queued before the terminal task action completed, \
         so the original coordinator instructions are stale.",
    );
    Some(msg)
}

pub(crate) fn active_review_matches(brehon_root: &Path, task_id: &str, review_id: &str) -> bool {
    let state_path = brehon_root
        .join("runtime")
        .join("reviews")
        .join(task_id)
        .join("state.json");
    let task_path = brehon_root
        .join("runtime")
        .join("tasks")
        .join(format!("{task_id}.json"));

    let Ok(state_content) = std::fs::read_to_string(&state_path) else {
        return false;
    };
    let Ok(task_content) = std::fs::read_to_string(&task_path) else {
        return false;
    };

    let Ok(state) = serde_json::from_str::<serde_json::Value>(&state_content) else {
        return false;
    };
    let Ok(task) = serde_json::from_str::<serde_json::Value>(&task_content) else {
        return false;
    };

    state
        .get("current_review_id")
        .and_then(|value| value.as_str())
        == Some(review_id)
        && state.get("status").and_then(|value| value.as_str()) == Some("collecting")
        && matches!(
            task.get("status").and_then(|value| value.as_str()),
            Some("in_review" | "InReview")
        )
}

pub(crate) fn should_drop_stale_review_prompt(brehon_root: &Path, prompt_text: &str) -> bool {
    let identity = extract_review_request_identity(prompt_text)
        .or_else(|| extract_review_timeout_identity(prompt_text));
    let Some((review_id, task_id)) = identity else {
        return false;
    };
    !active_review_matches(brehon_root, &task_id, &review_id)
}

fn task_completion_mode(raw: &serde_json::Map<String, serde_json::Value>) -> TaskCompletionMode {
    raw.get("completion_mode")
        .and_then(|value| value.as_str())
        .and_then(parse_task_completion_mode)
        .unwrap_or_else(|| {
            let title = raw
                .get("title")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let description = raw
                .get("description")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            infer_task_completion_mode(title, description)
        })
}

fn has_reviewable_commit(raw: &serde_json::Map<String, serde_json::Value>) -> bool {
    raw.get("latest_commit")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
}

fn can_auto_transition_to_review_ready(raw: &serde_json::Map<String, serde_json::Value>) -> bool {
    task_completion_mode(raw) == TaskCompletionMode::Close || has_reviewable_commit(raw)
}

// ── Worker recovery ─────────────────────────────────────────────────────────

pub(crate) fn candidate_worker_worktree_paths(
    brehon_root: &std::path::Path,
    worker_name: &str,
) -> Vec<PathBuf> {
    let worktrees_dir = brehon_root.join("worktrees");
    let mut candidates = Vec::new();

    let legacy = worktrees_dir.join(worker_name);
    if legacy.is_dir() {
        candidates.push(legacy);
    }

    let runs_dir = worktrees_dir.join("runs");
    if let Ok(run_entries) = std::fs::read_dir(&runs_dir) {
        for run_entry in run_entries.flatten() {
            let run_path = run_entry.path();
            if !run_path.is_dir() {
                continue;
            }
            let candidate = run_path.join(worker_name);
            if candidate.is_dir() {
                candidates.push(candidate);
            }
        }
    }

    candidates
}

pub(crate) fn inspect_worker_worktree_state(
    brehon_root: &std::path::Path,
    worker_name: &str,
) -> WorkerWorktreeInspection {
    let candidates = candidate_worker_worktree_paths(brehon_root, worker_name);
    if candidates.is_empty() {
        return WorkerWorktreeInspection::Missing;
    }
    if candidates.len() > 1 {
        let paths = candidates
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return WorkerWorktreeInspection::Dirty(format!("ambiguous worktree candidates: {paths}"));
    }

    let worktree = candidates.into_iter().next().unwrap();
    let unmerged = Command::new("git")
        .arg("-C")
        .arg(&worktree)
        .args(["diff", "--name-only", "--diff-filter=U"])
        .output();
    match unmerged {
        Ok(output) if output.status.success() => {
            let files = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            if !files.is_empty() {
                return WorkerWorktreeInspection::Unmerged { files };
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return WorkerWorktreeInspection::Dirty(if stderr.is_empty() {
                format!("git diff --diff-filter=U exited with {}", output.status)
            } else {
                stderr
            });
        }
        Err(err) => {
            return WorkerWorktreeInspection::Dirty(format!(
                "failed to inspect worktree conflicts: {err}"
            ));
        }
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(&worktree)
        .args(["status", "--porcelain"])
        .output();
    let output = match output {
        Ok(output) => output,
        Err(err) => {
            return WorkerWorktreeInspection::Dirty(format!("git status failed: {err}"));
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return WorkerWorktreeInspection::Dirty(if stderr.is_empty() {
            format!("git status exited with {}", output.status)
        } else {
            stderr
        });
    }
    let dirty = !String::from_utf8_lossy(&output.stdout).trim().is_empty();
    if dirty {
        return WorkerWorktreeInspection::Dirty("worktree has uncommitted changes".to_string());
    }
    WorkerWorktreeInspection::Clean
}

pub(crate) fn escalate_worker_unmerged_conflict(
    brehon_root: &std::path::Path,
    task_id: &str,
    worker_name: &str,
    raw: &mut serde_json::Map<String, serde_json::Value>,
    conflicting_files: &[String],
    idle_minutes: u64,
) -> Result<(), String> {
    let merge_target = raw
        .get("merge_target")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("unknown merge target")
        .to_string();
    let reviewed_commit = raw
        .get("latest_commit")
        .and_then(|value| value.as_str())
        .or_else(|| raw.get("merged_commit").and_then(|value| value.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown commit")
        .to_string();
    let files = if conflicting_files.is_empty() {
        vec!["unknown files".to_string()]
    } else {
        conflicting_files.to_vec()
    };
    let now = chrono::Utc::now().to_rfc3339();

    raw.insert(
        "status".into(),
        serde_json::Value::String("changes_requested".to_string()),
    );
    raw.insert("assignee".into(), serde_json::Value::Null);
    raw.insert("review_owner".into(), serde_json::Value::Null);
    raw.insert(
        "activity".into(),
        serde_json::Value::String("integration_conflict".to_string()),
    );
    raw.insert(
        "blockers".into(),
        serde_json::Value::String(format!(
            "Supervisor-owned integration conflict for reviewed commit {reviewed_commit} against '{merge_target}'. Conflicting files: {}. Supervisor must inspect the stranded worker worktree and decide how to continue before the task can be reassigned.",
            files.join(", ")
        )),
    );
    raw.insert(
        "integration_conflict".into(),
        serde_json::json!({
            "owner": "supervisor",
            "source": "worker_unmerged",
            "merge_target": merge_target,
            "reviewed_commit": reviewed_commit,
            "reviewed_commits": if reviewed_commit == "unknown commit" {
                Vec::<String>::new()
            } else {
                vec![reviewed_commit.clone()]
            },
            "conflicting_files": files,
            "previous_worker": worker_name,
            "recorded_at": now,
        }),
    );
    raw.insert(
        "recovery_note".into(),
        serde_json::Value::String(format!(
            "Automatically escalated to supervisor-owned integration conflict after {idle_minutes} minutes without pane output. Worker {worker_name} was left with an unmerged index."
        )),
    );
    raw.insert("updated_at".into(), serde_json::Value::String(now));

    write_raw_task_file(brehon_root, task_id, raw)
}

pub(crate) fn detect_shared_root_mutation(brehon_root: &std::path::Path) -> Option<String> {
    let project_root = brehon_root.parent()?;
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()?;
    if !output.status.success() {
        return Some(format!(
            "failed to inspect shared repo root '{}' for unexpected mutations",
            project_root.display()
        ));
    }

    let dirty = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(5)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if dirty.is_empty() {
        None
    } else {
        Some(format!(
            "shared repo root '{}' is dirty during run: {}{}",
            project_root.display(),
            dirty.join(", "),
            if dirty.len() == 5 { ", ..." } else { "" }
        ))
    }
}

// ── Task file operations ────────────────────────────────────────────────────

pub(crate) fn read_raw_task_file(
    brehon_root: &std::path::Path,
    task_id: &str,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let path = brehon_root
        .join("runtime")
        .join("tasks")
        .join(format!("{task_id}.json"));
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub(crate) fn write_raw_task_file(
    brehon_root: &std::path::Path,
    task_id: &str,
    task: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).map_err(|err| err.to_string())?;
    let path = tasks_dir.join(format!("{task_id}.json"));
    let tmp = tasks_dir.join(format!(".{task_id}.tmp"));
    let data = serde_json::to_string_pretty(&serde_json::Value::Object(task.clone()))
        .map_err(|err| err.to_string())?;
    std::fs::write(&tmp, data).map_err(|err| err.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|err| err.to_string())
}

pub(crate) fn promote_active_assigned_task(
    brehon_root: &std::path::Path,
    task_id: &str,
    worker_name: &str,
) -> Option<&'static str> {
    let mut raw = read_raw_task_file(brehon_root, task_id)?;
    let current_status = raw
        .get("status")
        .and_then(|value| value.as_str())
        .unwrap_or("pending");
    if normalize_task_status(current_status) != Some("assigned") {
        return None;
    }

    let assignee = raw
        .get("assignee")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    if assignee != worker_name {
        return None;
    }

    let percent = raw
        .get("percent")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let now = chrono::Utc::now().to_rfc3339();

    let new_status = if percent >= 100 && can_auto_transition_to_review_ready(&raw) {
        let review_owner = raw
            .get("review_owner")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from)
            .unwrap_or_else(|| worker_name.to_string());
        raw.insert(
            "review_owner".into(),
            serde_json::Value::String(review_owner),
        );
        "review_ready"
    } else {
        "in_progress"
    };

    raw.insert(
        "status".into(),
        serde_json::Value::String(new_status.to_string()),
    );
    raw.insert("updated_at".into(), serde_json::Value::String(now));

    if write_raw_task_file(brehon_root, task_id, &raw).is_err() {
        return None;
    }

    Some(new_status)
}

pub(crate) fn task_is_worker_owned(task: &TaskInfo) -> bool {
    if task.integration_conflict_owner.as_deref() == Some("supervisor") {
        return false;
    }

    matches!(
        normalize_task_status(&task.status),
        Some("assigned" | "in_progress" | "changes_requested")
    )
}

pub(crate) fn task_reserves_worker(task: &TaskInfo) -> bool {
    if task.integration_conflict_owner.as_deref() == Some("supervisor") {
        return false;
    }

    matches!(
        normalize_task_status(&task.status),
        Some(
            "assigned"
                | "in_progress"
                | "review_ready"
                | "in_review"
                | "changes_requested"
                | "approved"
        )
    )
}

pub(crate) fn active_worker_task(task: &TaskInfo, worker_name: &str) -> bool {
    task.assignee.as_deref() == Some(worker_name) && task_is_worker_owned(task)
}

pub(crate) fn idle_worker_names(
    tasks: &[TaskInfo],
    sessions: &std::collections::HashMap<String, (String, String, String)>,
    excluding_worker: &str,
) -> Vec<String> {
    let busy: std::collections::HashSet<&str> = tasks
        .iter()
        .filter(|task| task_reserves_worker(task))
        .filter_map(|task| task.assignee.as_deref())
        .collect();

    let mut workers = sessions
        .iter()
        .filter_map(|(name, (role, _, _))| {
            (role == "worker" && name != excluding_worker && !busy.contains(name.as_str()))
                .then_some(name.clone())
        })
        .collect::<Vec<_>>();
    workers.sort();
    workers
}

pub(crate) fn quarantined_worker_names(
    brehon_root: &std::path::Path,
    tasks: &[TaskInfo],
    sessions: &std::collections::HashMap<String, (String, String, String)>,
) -> Vec<String> {
    let mut workers = sessions
        .iter()
        .filter_map(|(name, (role, _, _))| {
            (role == "worker"
                && agent_is_quarantined_for_run(brehon_root, name)
                && tasks.iter().any(|task| active_worker_task(task, name)))
            .then_some(name.clone())
        })
        .collect::<Vec<_>>();
    workers.sort();
    workers
}

pub(crate) fn attempt_auto_recover_stalled_worker(
    brehon_root: &std::path::Path,
    worker_name: &str,
    tasks: &[TaskInfo],
    sessions: &std::collections::HashMap<String, (String, String, String)>,
    idle_minutes: u64,
) -> Option<StalledRecoveryOutcome> {
    let task = tasks
        .iter()
        .find(|task| active_worker_task(task, worker_name))?;
    let task_id = task.id.clone();

    match inspect_worker_worktree_state(brehon_root, worker_name) {
        WorkerWorktreeInspection::Missing | WorkerWorktreeInspection::Clean => {}
        WorkerWorktreeInspection::Dirty(reason) => {
            return Some(StalledRecoveryOutcome::Blocked {
                task_id,
                worker: worker_name.to_string(),
                reason,
            });
        }
        WorkerWorktreeInspection::Unmerged { files } => {
            let mut raw = read_raw_task_file(brehon_root, &task.id)?;
            if escalate_worker_unmerged_conflict(
                brehon_root,
                &task.id,
                worker_name,
                &mut raw,
                &files,
                idle_minutes,
            )
            .is_err()
            {
                return None;
            }
            return Some(StalledRecoveryOutcome::SupervisorConflict {
                task_id,
                worker: worker_name.to_string(),
                files,
            });
        }
    }

    let mut raw = read_raw_task_file(brehon_root, &task.id)?;
    let current_status = raw
        .get("status")
        .and_then(|value| value.as_str())
        .unwrap_or("pending");
    let normalized_status = normalize_task_status(current_status).unwrap_or("pending");
    let replacement = idle_worker_names(tasks, sessions, worker_name)
        .into_iter()
        .next();
    let now = chrono::Utc::now().to_rfc3339();

    if task.percent.is_some_and(|percent| percent >= 100)
        && can_auto_transition_to_review_ready(&raw)
    {
        let review_owner = raw
            .get("review_owner")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from)
            .or_else(|| {
                raw.get("assignee")
                    .and_then(|value| value.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(String::from)
            })
            .unwrap_or_else(|| worker_name.to_string());

        raw.insert(
            "status".into(),
            serde_json::Value::String("review_ready".to_string()),
        );
        raw.insert(
            "assignee".into(),
            serde_json::Value::String(review_owner.clone()),
        );
        raw.insert(
            "review_owner".into(),
            serde_json::Value::String(review_owner),
        );
        raw.insert("updated_at".into(), serde_json::Value::String(now));
        raw.insert(
            "recovery_note".into(),
            serde_json::Value::String(format!(
                "Automatically normalized stalled task after {idle_minutes} minutes without pane output. Recorded progress was already at 100%, so the task was moved to review_ready while preserving worker ownership until the task is completed."
            )),
        );

        if write_raw_task_file(brehon_root, &task.id, &raw).is_err() {
            return None;
        }

        return Some(StalledRecoveryOutcome::ReviewReady {
            task_id: task.id.clone(),
            worker: worker_name.to_string(),
        });
    }

    let recovery_note = match replacement.as_deref() {
        Some(new_worker) => format!(
            "Automatically recovered stalled task after {idle_minutes} minutes without pane output. Previous worker {worker_name} had a clean worktree. Reassigned to {new_worker}."
        ),
        None => format!(
            "Automatically reclaimed stalled task after {idle_minutes} minutes without pane output. Previous worker {worker_name} had a clean worktree. Returned to queue."
        ),
    };

    match replacement.as_deref() {
        Some(new_worker) => {
            raw.insert(
                "assignee".into(),
                serde_json::Value::String(new_worker.to_string()),
            );
            raw.insert(
                "status".into(),
                serde_json::Value::String("assigned".to_string()),
            );
            raw.insert("review_owner".into(), serde_json::Value::Null);
        }
        None => {
            raw.insert("assignee".into(), serde_json::Value::Null);
            let next_status = if normalized_status == "changes_requested" {
                "changes_requested"
            } else {
                "pending"
            };
            raw.insert(
                "status".into(),
                serde_json::Value::String(next_status.to_string()),
            );
            raw.insert("review_owner".into(), serde_json::Value::Null);
        }
    }
    raw.insert("updated_at".into(), serde_json::Value::String(now));
    raw.insert(
        "recovery_note".into(),
        serde_json::Value::String(recovery_note),
    );

    if write_raw_task_file(brehon_root, &task.id, &raw).is_err() {
        return None;
    }

    Some(match replacement {
        Some(new_worker) => StalledRecoveryOutcome::Reassigned {
            task_id: task.id.clone(),
            old_worker: worker_name.to_string(),
            new_worker,
        },
        None => StalledRecoveryOutcome::Requeued {
            task_id: task.id.clone(),
            old_worker: worker_name.to_string(),
        },
    })
}

// ── Task context ────────────────────────────────────────────────────────────

pub(crate) fn pick_blocked_reason(
    task: &TaskInfo,
    tasks_by_id: &std::collections::HashMap<&str, &TaskInfo>,
) -> Option<TaskBlockedReason> {
    let blocker_task_id = task
        .blocked_by
        .iter()
        .find(|id| !id.is_empty())
        .map(ToOwned::to_owned);

    let summary = match (
        task.blockers
            .as_ref()
            .filter(|text| !text.trim().is_empty()),
        blocker_task_id.as_deref(),
    ) {
        (Some(text), _) => Some(text.clone()),
        (None, Some(blocker_id)) => tasks_by_id
            .get(blocker_id)
            .map(|blocked| format!("Waiting on {}: {}", blocked.id, blocked.title)),
        (None, None) => None,
    };

    if blocker_task_id.is_none() && summary.is_none() {
        None
    } else {
        Some(TaskBlockedReason {
            blocker_task_id,
            summary,
        })
    }
}

pub(crate) fn build_task_context_snapshot(
    task: &TaskInfo,
    tasks_by_id: &std::collections::HashMap<&str, &TaskInfo>,
) -> TaskContextSnapshot {
    let parent_epic = task
        .parent_id
        .as_deref()
        .and_then(|parent_id| tasks_by_id.get(parent_id).copied());

    let epic_branch = parent_epic
        .and_then(|epic| epic.integration_branch.clone())
        .or_else(|| task.merge_target.clone());
    let epic_worktree = parent_epic
        .and_then(|epic| epic.integration_worktree.clone())
        .map(std::path::PathBuf::from);

    let blocked_reason = if normalize_task_status(&task.status) == Some("blocked") {
        pick_blocked_reason(task, tasks_by_id)
    } else {
        None
    };

    TaskContextSnapshot::from_task(
        &task_info_to_task(task),
        TaskContextDetails {
            completion_mode: task.completion_mode.clone(),
            merge_target: task.merge_target.clone(),
            parent_id: task.parent_id.clone(),
            epic_branch,
            epic_worktree,
            blocked_reason,
        },
    )
}

pub(crate) fn sync_worker_task_contexts(
    mux: &mut Mux,
    tasks: &[TaskInfo],
    sessions: &std::collections::HashMap<String, (String, String, String)>,
) {
    let tasks_by_id: std::collections::HashMap<&str, &TaskInfo> =
        tasks.iter().map(|task| (task.id.as_str(), task)).collect();
    let mut active_by_assignee: std::collections::HashMap<&str, &TaskInfo> =
        std::collections::HashMap::new();

    for task in tasks {
        if task.assignee.is_none() || task_is_terminal(task) {
            continue;
        }
        let Some(assignee) = task.assignee.as_deref() else {
            continue;
        };
        let should_replace = active_by_assignee
            .get(assignee)
            .and_then(|current| {
                let current_updated = current.updated_at.as_deref().unwrap_or_default();
                let candidate_updated = task.updated_at.as_deref().unwrap_or_default();
                if candidate_updated > current_updated {
                    Some(true)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| !active_by_assignee.contains_key(assignee));

        if should_replace {
            active_by_assignee.insert(assignee, task);
        }
    }

    let worker_session_map: Vec<(String, String)> = mux
        .panes()
        .filter(|pane| *pane.kind() == PaneKind::Worker)
        .filter_map(|pane| {
            let sid = pane
                .agent_session_id()
                .map(ToOwned::to_owned)
                .or_else(|| sessions.get(pane.id()).map(|(_, sid, _)| sid.clone()))?;
            Some((pane.id().to_string(), sid))
        })
        .collect();

    for (worker_name, session_id) in worker_session_map {
        if let Some(task) = active_by_assignee.get(worker_name.as_str()) {
            let snapshot = build_task_context_snapshot(task, &tasks_by_id);
            if snapshot.is_terminal() {
                mux.clear_pane_task_context_by_session(&session_id);
            } else {
                mux.set_pane_task_context_by_session(&session_id, snapshot);
            }
        } else {
            mux.clear_pane_task_context_by_session(&session_id);
        }
    }
}

// ── Task conversion ─────────────────────────────────────────────────────────

pub(crate) fn parse_task_status(status: &str) -> TaskStatus {
    match normalize_task_status(status) {
        Some("assigned") => TaskStatus::Assigned,
        Some("in_progress") => TaskStatus::InProgress,
        Some("review_ready") => TaskStatus::InReview,
        Some("in_review") => TaskStatus::InReview,
        Some("changes_requested") => TaskStatus::ChangesRequested,
        Some("approved") => TaskStatus::Approved,
        Some("merged") | Some("closed") => TaskStatus::Merged,
        Some("blocked") => TaskStatus::Blocked,
        _ => TaskStatus::Pending,
    }
}

pub(crate) fn parse_priority(value: Option<&str>) -> Priority {
    match value.unwrap_or_default().to_ascii_lowercase().as_str() {
        "critical" => Priority::Critical,
        "high" => Priority::High,
        "low" => Priority::Low,
        _ => Priority::Medium,
    }
}

pub(crate) fn task_info_to_task(task: &TaskInfo) -> Task {
    let now = chrono::Utc::now();
    Task {
        id: TaskId::new(task.id.clone()),
        title: task.title.clone(),
        description: task.description.clone(),
        status: parse_task_status(&task.status),
        priority: parse_priority(task.priority.as_deref()),
        assignee: task.assignee.clone(),
        dependencies: task
            .dependencies
            .iter()
            .map(|id| TaskId::new(id.clone()))
            .collect(),
        created_at: now,
        updated_at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_dead_letter(target: &str, message: &str) -> DeadLetterEntry {
        DeadLetterEntry {
            original_path: format!("/tmp/{target}.prompt"),
            target: target.to_string(),
            from: Some("supervisor".to_string()),
            message: message.to_string(),
            prompt_id: Some(format!("prompt-{target}")),
            error: "transport failed".to_string(),
            reason: "nonrecoverable prompt delivery failure".to_string(),
            dead_lettered_at: "2026-04-23T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn read_queued_prompt_accepts_session_scoped_entry_envelope() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("queued.entry");
        let payload = serde_json::json!({
            "session_name": "session-a",
            "entry": {
                "target": "worker-1",
                "from": "supervisor",
                "message": "please continue",
                "prompt_id": "prompt-123"
            }
        });
        std::fs::write(&path, serde_json::to_vec(&payload).unwrap()).expect("write prompt");

        let queued = read_queued_prompt(&path).expect("parse scoped prompt");
        assert_eq!(queued.session_name.as_deref(), Some("session-a"));
        assert_eq!(queued.target, "worker-1");
        assert_eq!(queued.from.as_deref(), Some("supervisor"));
        assert_eq!(queued.prompt_id.as_deref(), Some("prompt-123"));
        assert_eq!(queued.message, "please continue");
    }

    #[test]
    fn prompt_queue_sweep_dirs_include_scoped_root_and_legacy_dirs() {
        let temp = TempDir::new().expect("tempdir");
        let dirs = runtime_prompt_queue_sweep_dirs(temp.path(), Some("session-a"));
        assert_eq!(dirs[0], temp.path().join("runtime").join("prompt-queue"));
        assert!(dirs.contains(
            &temp
                .path()
                .join("runtime")
                .join("prompt-queue")
                .join("session-a")
        ));
        assert!(dirs.contains(
            &temp
                .path()
                .join("runtime")
                .join("prompt-queue")
                .join("_legacy")
        ));
    }

    #[test]
    fn dead_letter_queue_round_trip_uses_tempdir() {
        let temp = TempDir::new().expect("tempdir");
        let queue_dir = temp.path().join("prompt-dead-letter");
        let queue = dead_letter_queue_for_session(Some("session-a"), queue_dir);
        let payload = sample_dead_letter("worker-a", "hello from a");

        let entry_id = queue.enqueue(payload.clone()).expect("enqueue");
        let drained: Vec<_> = queue.drain().collect();

        assert_eq!(drained.len(), 1);
        let entry = drained[0].as_ref().expect("drain entry should be ok");
        assert_eq!(entry.id, entry_id);
        assert_eq!(entry.session_name, "session-a");
        assert_eq!(entry.entry, payload);
    }

    #[test]
    fn dead_letter_queue_filters_cross_session_and_tool_listing_shows_prior_session() {
        let temp = TempDir::new().expect("tempdir");
        let queue_dir = temp.path().join("prompt-dead-letter");
        let queue_a = dead_letter_queue_for_session(Some("session-a"), queue_dir.clone());
        queue_a
            .enqueue(sample_dead_letter(
                "worker-a",
                "hello from previous session",
            ))
            .expect("enqueue session-a dead letter");

        let queue_b = dead_letter_queue_for_session(Some("session-b"), queue_dir.clone());
        let drained_b: Vec<_> = queue_b.drain().collect();
        assert!(
            drained_b.is_empty(),
            "session-b should not replay session-a dead letters"
        );

        let visible = list_dead_letters_for_tool(&queue_dir);
        assert_eq!(visible.len(), 1, "tool listing should expose prior session");
        assert_eq!(visible[0].session_name, "session-a");
        assert_eq!(visible[0].entry.target, "worker-a");
        assert_eq!(visible[0].entry.message, "hello from previous session");
    }
}

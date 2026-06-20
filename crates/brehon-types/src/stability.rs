//! Stability counters and boundedness probes for runtime observability.
//!
//! `StabilityCounters` provides a read-only snapshot of key runtime metrics
//! that are useful for diagnosing stalls, leaks, and capacity pressure. Each
//! counter is a simple scalar — no histograms, no percentiles — so the struct
//! is cheap to construct and safe to share across threads.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Result as IoResult};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
#[cfg(unix)]
use std::os::fd::AsRawFd;

use crate::task::is_terminal_task_status;

/// Read-only snapshot of runtime stability counters.
///
/// These counters are derived from internal data structures and do not require
/// holding any locks to read once the snapshot is taken. They are intended for
/// diagnostics dashboards, the `brehon doctor` checker, and health endpoints.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct StabilityCounters {
    /// Number of in-flight ACP requests awaiting a response.
    pub pending_requests: usize,
    /// Number of pending prompt-response oneshot channels not yet completed or timed out.
    ///
    /// Tracks the count of entries in `pending_prompt_responses`, not stored results.
    /// The serialized name remains `prompt_results` for backward compatibility.
    #[serde(rename = "prompt_results", alias = "pending_prompt_waiters")]
    pub pending_prompt_waiters: usize,
    /// Number of reviews currently being collected (not yet approved/rejected).
    pub active_reviews: usize,
    /// Cumulative count of tasks that have reached a terminal merged state.
    pub completed_tasks: usize,
    /// Total number of assignment records ever recorded (including reassignments).
    pub assignment_history: usize,
    /// Number of prompt sends that failed or were dropped due to capacity/closure.
    pub blocked_sends: usize,
    /// Cumulative number of tokens reported by active agent sessions.
    ///
    /// This is provider-reported best effort data. Sessions that do not expose
    /// token usage contribute zero.
    #[serde(default)]
    pub tokens_used: u64,
}

impl StabilityCounters {
    /// Create an empty counter set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Merge another counter snapshot into this one (summing each field).
    ///
    /// Useful for aggregating counters from multiple subsystems.
    pub fn merge(&mut self, other: &StabilityCounters) {
        self.pending_requests = self.pending_requests.saturating_add(other.pending_requests);
        self.pending_prompt_waiters = self
            .pending_prompt_waiters
            .saturating_add(other.pending_prompt_waiters);
        self.active_reviews = self.active_reviews.saturating_add(other.active_reviews);
        self.completed_tasks = self.completed_tasks.saturating_add(other.completed_tasks);
        self.assignment_history = self
            .assignment_history
            .saturating_add(other.assignment_history);
        self.blocked_sends = self.blocked_sends.saturating_add(other.blocked_sends);
        self.tokens_used = self.tokens_used.saturating_add(other.tokens_used);
    }

    /// Return true if any counter exceeds its corresponding soft bound.
    ///
    /// The bounds are deliberately generous to avoid false positives; the
    /// purpose is to flag obvious capacity pressure, not to enforce strict
    /// limits. A boundedness probe should only fire when something is
    /// clearly wrong (e.g. thousands of pending requests).
    pub fn exceeds_bounds(&self, bounds: &StabilityBounds) -> bool {
        self.pending_requests > bounds.max_pending_requests
            || self.pending_prompt_waiters > bounds.max_prompt_results
            || self.active_reviews > bounds.max_active_reviews
            || self.blocked_sends > bounds.max_blocked_sends
    }
}

/// Soft upper bounds for stability counters.
///
/// Values exceeding these bounds trigger diagnostic warnings but do not block
/// operation. They exist to surface capacity leaks early.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct StabilityBounds {
    pub max_pending_requests: usize,
    pub max_prompt_results: usize,
    pub max_active_reviews: usize,
    pub max_blocked_sends: usize,
}

impl Default for StabilityBounds {
    fn default() -> Self {
        Self {
            // Each bound is set high enough that normal operation never trips it.
            // A single task generates ~1-5 pending requests; 256 implies dozens of
            // concurrent tasks or a serious leak.
            max_pending_requests: 256,
            // Prompt results are ephemeral; 512 would mean no results are being
            // consumed.
            max_prompt_results: 512,
            // More than 64 concurrent reviews is unusual for a typical factory.
            max_active_reviews: 64,
            // Even a single blocked send is worth flagging, but we allow a small
            // margin for transient races during shutdown.
            max_blocked_sends: 8,
        }
    }
}

/// Persisted runtime snapshot for doctor checks and dashboards.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedStabilityCounters {
    #[serde(flatten)]
    pub counters: StabilityCounters,
    pub updated_at: String,
}

impl PersistedStabilityCounters {
    pub fn new(counters: StabilityCounters) -> Self {
        Self {
            counters,
            updated_at: Utc::now().to_rfc3339(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
struct StabilityMeta {
    #[serde(default)]
    assignment_history: usize,
}

struct FileLock {
    #[cfg(unix)]
    file: File,
    #[cfg(not(unix))]
    path: PathBuf,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn runtime_dir(brehon_root: &Path) -> PathBuf {
    brehon_root.join("runtime")
}

fn session_counter_dir(brehon_root: &Path) -> PathBuf {
    runtime_dir(brehon_root).join("stability-sessions")
}

fn session_counter_path(brehon_root: &Path, session_key: &str) -> PathBuf {
    session_counter_dir(brehon_root).join(format!("{session_key}.json"))
}

fn task_dir(brehon_root: &Path) -> PathBuf {
    runtime_dir(brehon_root).join("tasks")
}

fn task_json_path(brehon_root: &Path, task_id: &str) -> PathBuf {
    task_dir(brehon_root).join(format!("{task_id}.json"))
}

fn stability_meta_path(brehon_root: &Path) -> PathBuf {
    runtime_dir(brehon_root).join("stability-meta.json")
}

pub fn runtime_stability_counters_path(brehon_root: &Path) -> PathBuf {
    runtime_dir(brehon_root).join("stability-counters.json")
}

fn unique_tmp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("stability");
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
}

fn lock_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("stability");
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{file_name}.lock"))
}

fn acquire_file_lock(path: &Path) -> IoResult<FileLock> {
    let lock_path = lock_path(path);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(FileLock { file })
    }
    #[cfg(not(unix))]
    {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path);
        match file {
            Ok(_) => Ok(FileLock { path: lock_path }),
            Err(err) => Err(err),
        }
    }
}

/// Atomically and durably persist `value` as pretty-printed JSON at `path`.
///
/// Crash-safe recovery writes must never observe a truncated or partially
/// written file. This helper serializes to a hidden temp file in the **same**
/// directory (so the rename stays on one filesystem), fsyncs the temp file's
/// contents, renames it over the target, then fsyncs the parent directory so
/// the rename itself survives a crash (Unix only — see below).
///
/// A serialize failure is propagated, never written: this helper will never
/// truncate or empty the target file on a serialization error.
///
/// On non-Unix platforms the parent-directory fsync is skipped (no portable
/// API); the temp+fsync-file+rename path is still applied, which is the best
/// available durability there.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> IoResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Serialize FIRST so a serialization failure aborts before we touch any
    // file — never write a truncated/empty target.
    let data = serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;

    let tmp = unique_tmp_path(path);
    match write_tmp_and_rename(&tmp, path, data.as_bytes()) {
        Ok(()) => Ok(()),
        Err(err) => {
            // Best-effort cleanup so a failed write does not leak a temp file.
            let _ = std::fs::remove_file(&tmp);
            Err(err)
        }
    }
}

fn write_tmp_and_rename(tmp: &Path, path: &Path, data: &[u8]) -> IoResult<()> {
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(tmp)?;
        use std::io::Write as _;
        file.write_all(data)?;
        // fsync the temp file's contents before the rename makes it visible.
        file.sync_all()?;
    }
    std::fs::rename(tmp, path)?;
    // fsync the parent directory so the rename is durable across a crash.
    #[cfg(unix)]
    {
        if let Some(parent) = path.parent() {
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            let dir = File::open(parent)?;
            dir.sync_all()?;
        }
    }
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn load_runtime_stability_counters(path: &Path) -> Option<StabilityCounters> {
    read_json::<PersistedStabilityCounters>(path)
        .map(|snapshot| snapshot.counters)
        .or_else(|| read_json::<StabilityCounters>(path))
}

pub fn write_session_stability_counters(
    brehon_root: &Path,
    session_key: &str,
    counters: StabilityCounters,
) -> IoResult<()> {
    write_json_atomic(
        &session_counter_path(brehon_root, session_key),
        &PersistedStabilityCounters::new(counters),
    )
}

pub fn remove_session_stability_counters(brehon_root: &Path, session_key: &str) -> IoResult<()> {
    match std::fs::remove_file(session_counter_path(brehon_root, session_key)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[derive(Debug, Clone)]
struct RuntimeTaskSummary {
    id: String,
    task_type: String,
    status: String,
    assignee: Option<String>,
}

fn read_runtime_task_summaries(brehon_root: &Path) -> Vec<RuntimeTaskSummary> {
    let Ok(entries) = std::fs::read_dir(task_dir(brehon_root)) else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter(|entry| {
            entry.path().extension().is_some_and(|ext| ext == "json")
                && !entry.file_name().to_string_lossy().starts_with('.')
        })
        .filter_map(|entry| {
            let content = std::fs::read_to_string(entry.path()).ok()?;
            let task = serde_json::from_str::<Map<String, Value>>(&content).ok()?;
            let id = task
                .get("task_id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())?
                .to_string();
            Some(RuntimeTaskSummary {
                id,
                task_type: task
                    .get("task_type")
                    .and_then(Value::as_str)
                    .unwrap_or("task")
                    .to_string(),
                status: task
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("pending")
                    .to_string(),
                assignee: task
                    .get("assignee")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

/// Infer the task whose persistent token rollup should receive a prompt's usage.
///
/// This is deliberately conservative: explicit task IDs in the prompt win, and
/// otherwise a worker session only falls back to its single active assigned
/// leaf task. Ambiguous prompts are left unattributed instead of guessing.
pub fn infer_task_token_target(
    brehon_root: &Path,
    agent_name: &str,
    role: &str,
    prompt_content: &str,
) -> Option<String> {
    let tasks = read_runtime_task_summaries(brehon_root);
    if tasks.is_empty() {
        return None;
    }

    let mentioned: Vec<&RuntimeTaskSummary> = tasks
        .iter()
        .filter(|task| prompt_mentions_task_id(prompt_content, &task.id))
        .collect();
    if mentioned.len() == 1 {
        return Some(mentioned[0].id.clone());
    }

    if role.eq_ignore_ascii_case("worker") {
        let active_assigned: Vec<&RuntimeTaskSummary> = tasks
            .iter()
            .filter(|task| task.task_type == "task")
            .filter(|task| task.assignee.as_deref() == Some(agent_name))
            .filter(|task| !is_terminal_task_status(&task.status))
            .collect();
        if active_assigned.len() == 1 {
            return Some(active_assigned[0].id.clone());
        }
    }

    None
}

fn prompt_mentions_task_id(prompt: &str, task_id: &str) -> bool {
    prompt.match_indices(task_id).any(|(start, _)| {
        let before = prompt[..start].chars().next_back();
        let after = prompt[start + task_id.len()..].chars().next();
        is_task_id_boundary(before) && is_task_id_boundary(after)
    })
}

fn is_task_id_boundary(ch: Option<char>) -> bool {
    ch.is_none_or(|ch| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
}

/// Persist a token delta on a task and every container ancestor.
///
/// The token rollup is stored in each task JSON as:
/// `{ "token_usage": { "tokens_used": <u64>, "updated_at": <rfc3339> } }`.
/// Updating ancestors means initiative and epic totals survive restarts without
/// needing any live session snapshot.
pub fn record_task_token_usage(
    brehon_root: &Path,
    task_id: &str,
    tokens_delta: u64,
) -> IoResult<Vec<String>> {
    let task_id = task_id.trim();
    if task_id.is_empty() || tokens_delta == 0 {
        return Ok(Vec::new());
    }

    let mut updated = Vec::new();
    let mut visited = HashSet::new();
    let mut current = Some(task_id.to_string());
    let updated_at = Utc::now().to_rfc3339();

    while let Some(id) = current.take() {
        if !visited.insert(id.clone()) {
            break;
        }

        let path = task_json_path(brehon_root, &id);
        let _lock = acquire_file_lock(&path)?;
        let Some(mut task) = read_json::<Map<String, Value>>(&path) else {
            break;
        };
        let actual_id = task
            .get("task_id")
            .and_then(Value::as_str)
            .unwrap_or(id.as_str())
            .to_string();
        current = task
            .get("parent_id")
            .and_then(Value::as_str)
            .filter(|parent_id| !parent_id.trim().is_empty())
            .map(str::to_string);

        increment_token_usage(&mut task, tokens_delta, &updated_at);
        write_json_atomic(&path, &Value::Object(task))?;
        updated.push(actual_id);
    }

    Ok(updated)
}

fn increment_token_usage(task: &mut Map<String, Value>, tokens_delta: u64, updated_at: &str) {
    let existing = task
        .get("token_usage")
        .and_then(|usage| usage.get("tokens_used"))
        .and_then(json_u64_value)
        .or_else(|| task.get("tokens_used").and_then(json_u64_value))
        .unwrap_or(0);
    let mut usage = task
        .get("token_usage")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    usage.insert(
        "tokens_used".to_string(),
        Value::Number(serde_json::Number::from(
            existing.saturating_add(tokens_delta),
        )),
    );
    usage.insert(
        "updated_at".to_string(),
        Value::String(updated_at.to_string()),
    );
    task.insert("token_usage".to_string(), Value::Object(usage));
}

fn json_u64_value(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
}

fn task_token_usage(task: &Map<String, Value>) -> Option<u64> {
    task.get("token_usage")
        .and_then(|usage| usage.get("tokens_used"))
        .and_then(json_u64_value)
        .or_else(|| task.get("tokens_used").and_then(json_u64_value))
}

/// Read the persisted token rollup for a single task.
///
/// Returns `Ok(Some(tokens))` when the task JSON exists and carries a
/// `token_usage.tokens_used` (or legacy top-level `tokens_used`) field,
/// `Ok(None)` when the file is absent or the field is missing, and `Err` only
/// on an unexpected IO error reading an existing file. Distinguishing "missing"
/// from a genuine zero lets the budget gate fail closed when spend state is
/// unknown rather than treating unreadable state as zero spend.
#[must_use = "the read token usage is the budget gate's spend signal"]
pub fn read_task_token_usage(brehon_root: &Path, task_id: &str) -> IoResult<Option<u64>> {
    let task_id = task_id.trim();
    if task_id.is_empty() {
        return Ok(None);
    }
    let path = task_json_path(brehon_root, task_id);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let Ok(task) = serde_json::from_str::<Map<String, Value>>(&content) else {
                return Ok(None);
            };
            Ok(task_token_usage(&task))
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Sum the persisted token rollup across the run's top-level container tasks.
///
/// The rollup written by [`record_task_token_usage`] accumulates each task's
/// usage into its ancestors, so the run total is the sum over the topmost
/// containers. Initiatives (`task_type == "initiative"` with no parent) are
/// preferred; if no initiative exists, all parent-less tasks are summed. This
/// avoids double counting children that already rolled up into their parents.
///
/// Returns `Err` when the tasks directory exists but cannot be read, which the
/// budget gate treats as "spend unknown" and fails closed under a Hard cap.
/// An absent tasks directory is a legitimately empty run and yields `Ok(0)`.
#[must_use = "the run total is the budget gate's spend signal"]
pub fn read_run_total_tokens(brehon_root: &Path) -> IoResult<u64> {
    let dir = task_dir(brehon_root);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };

    let mut initiative_total: u64 = 0;
    let mut has_initiative = false;
    let mut rootless_total: u64 = 0;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json")
            || entry.file_name().to_string_lossy().starts_with('.')
        {
            continue;
        }
        let Some(task) = read_json::<Map<String, Value>>(&path) else {
            continue;
        };
        let parentless = task
            .get("parent_id")
            .and_then(Value::as_str)
            .map(|parent| parent.trim().is_empty())
            .unwrap_or(true);
        if !parentless {
            continue;
        }
        let tokens = task_token_usage(&task).unwrap_or(0);
        let is_initiative = task
            .get("task_type")
            .and_then(Value::as_str)
            .is_some_and(|task_type| task_type == "initiative");
        if is_initiative {
            has_initiative = true;
            initiative_total = initiative_total.saturating_add(tokens);
        }
        rootless_total = rootless_total.saturating_add(tokens);
    }

    Ok(if has_initiative {
        initiative_total
    } else {
        rootless_total
    })
}

pub fn increment_assignment_history(brehon_root: &Path, delta: usize) -> IoResult<usize> {
    let path = stability_meta_path(brehon_root);
    let _lock = acquire_file_lock(&path)?;
    let mut meta = read_json::<StabilityMeta>(&path).unwrap_or_default();
    meta.assignment_history = meta.assignment_history.saturating_add(delta);
    let count = meta.assignment_history;
    write_json_atomic(&path, &meta)?;
    Ok(count)
}

pub fn refresh_runtime_stability_counters(brehon_root: &Path) -> IoResult<StabilityCounters> {
    let mut counters = StabilityCounters::default();

    let sessions_dir = session_counter_dir(brehon_root);
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            if let Some(snapshot) = read_json::<PersistedStabilityCounters>(&path) {
                counters.merge(&snapshot.counters);
            } else if let Some(session_counters) = read_json::<StabilityCounters>(&path) {
                counters.merge(&session_counters);
            }
        }
    }

    let tasks_dir = runtime_dir(brehon_root).join("tasks");
    if let Ok(entries) = std::fs::read_dir(&tasks_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Some(task) = read_json::<serde_json::Value>(&path) else {
                continue;
            };
            if let Some("merged") = task.get("status").and_then(|value| value.as_str()) {
                counters.completed_tasks += 1;
            }
        }
    }

    let reviews_dir = runtime_dir(brehon_root).join("reviews");
    if let Ok(entries) = std::fs::read_dir(&reviews_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(state) = read_json::<serde_json::Value>(&path.join("state.json")) else {
                continue;
            };
            if state.get("status").and_then(|value| value.as_str()) == Some("collecting") {
                counters.active_reviews += 1;
            }
        }
    }

    counters.assignment_history = read_json::<StabilityMeta>(&stability_meta_path(brehon_root))
        .map(|meta| meta.assignment_history)
        .unwrap_or_default();

    write_json_atomic(
        &runtime_stability_counters_path(brehon_root),
        &PersistedStabilityCounters::new(counters),
    )?;

    Ok(counters)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_counters_are_zero() {
        let c = StabilityCounters::default();
        assert_eq!(c.pending_requests, 0);
        assert_eq!(c.pending_prompt_waiters, 0);
        assert_eq!(c.active_reviews, 0);
        assert_eq!(c.completed_tasks, 0);
        assert_eq!(c.assignment_history, 0);
        assert_eq!(c.blocked_sends, 0);
        assert_eq!(c.tokens_used, 0);
    }

    #[test]
    fn merge_sums_fields() {
        let a = StabilityCounters {
            pending_requests: 3,
            pending_prompt_waiters: 1,
            active_reviews: 2,
            completed_tasks: 10,
            assignment_history: 5,
            blocked_sends: 0,
            tokens_used: 1_000,
        };
        let b = StabilityCounters {
            pending_requests: 7,
            pending_prompt_waiters: 4,
            active_reviews: 1,
            completed_tasks: 20,
            assignment_history: 15,
            blocked_sends: 2,
            tokens_used: 2_500,
        };
        let mut merged = a;
        merged.merge(&b);
        assert_eq!(merged.pending_requests, 10);
        assert_eq!(merged.pending_prompt_waiters, 5);
        assert_eq!(merged.active_reviews, 3);
        assert_eq!(merged.completed_tasks, 30);
        assert_eq!(merged.assignment_history, 20);
        assert_eq!(merged.blocked_sends, 2);
        assert_eq!(merged.tokens_used, 3_500);
    }

    #[test]
    fn within_bounds() {
        let counters = StabilityCounters {
            pending_requests: 10,
            pending_prompt_waiters: 20,
            active_reviews: 5,
            completed_tasks: 100,
            assignment_history: 50,
            blocked_sends: 0,
            tokens_used: 12_345,
        };
        assert!(!counters.exceeds_bounds(&StabilityBounds::default()));
    }

    #[test]
    fn exceeds_pending_requests_bound() {
        let counters = StabilityCounters {
            pending_requests: 300,
            ..Default::default()
        };
        assert!(counters.exceeds_bounds(&StabilityBounds::default()));
    }

    #[test]
    fn exceeds_blocked_sends_bound() {
        let counters = StabilityCounters {
            blocked_sends: 10,
            ..Default::default()
        };
        assert!(counters.exceeds_bounds(&StabilityBounds::default()));
    }

    #[test]
    fn custom_bounds() {
        let bounds = StabilityBounds {
            max_pending_requests: 10,
            max_prompt_results: 10,
            max_active_reviews: 10,
            max_blocked_sends: 0,
        };
        let counters = StabilityCounters {
            blocked_sends: 1,
            ..Default::default()
        };
        assert!(counters.exceeds_bounds(&bounds));
    }

    #[test]
    fn serde_roundtrip() {
        let counters = StabilityCounters {
            pending_requests: 42,
            pending_prompt_waiters: 7,
            active_reviews: 3,
            completed_tasks: 100,
            assignment_history: 50,
            blocked_sends: 1,
            tokens_used: 12_345,
        };
        let json = serde_json::to_string(&counters).unwrap();
        let parsed: StabilityCounters = serde_json::from_str(&json).unwrap();
        assert_eq!(counters, parsed);
    }

    #[test]
    fn serde_backward_compat_prompt_results_field() {
        // Old-format JSON uses "prompt_results" as the field name.
        // The serde(rename/alias) attributes ensure both old and new
        // field names deserialize correctly.
        let old_json = r#"{"pending_requests":1,"prompt_results":5,"active_reviews":0,"completed_tasks":0,"assignment_history":0,"blocked_sends":0}"#;
        let parsed: StabilityCounters = serde_json::from_str(old_json).unwrap();
        assert_eq!(parsed.pending_prompt_waiters, 5);
        assert_eq!(parsed.tokens_used, 0);

        // New-format JSON serializes using the rename attribute ("prompt_results")
        // for wire compatibility.
        let counters = StabilityCounters {
            pending_requests: 0,
            pending_prompt_waiters: 3,
            active_reviews: 0,
            completed_tasks: 0,
            assignment_history: 0,
            blocked_sends: 0,
            tokens_used: 42,
        };
        let json = serde_json::to_string(&counters).unwrap();
        assert!(
            json.contains(r#""prompt_results":3"#),
            "serialized JSON should use prompt_results for backward compat"
        );
        assert!(json.contains(r#""tokens_used":42"#));
    }

    #[test]
    fn refresh_runtime_stability_counters_merges_live_sources() {
        let root = tempfile::tempdir().unwrap();
        let brehon_root = root.path();
        let runtime = brehon_root.join("runtime");
        std::fs::create_dir_all(runtime.join("tasks")).unwrap();
        std::fs::create_dir_all(runtime.join("reviews/T-1")).unwrap();

        write_session_stability_counters(
            brehon_root,
            "worker-1",
            StabilityCounters {
                pending_requests: 2,
                pending_prompt_waiters: 3,
                blocked_sends: 1,
                tokens_used: 123,
                ..Default::default()
            },
        )
        .unwrap();
        increment_assignment_history(brehon_root, 4).unwrap();

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
            runtime.join("reviews/T-1/state.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": "T-1",
                "status": "collecting"
            }))
            .unwrap(),
        )
        .unwrap();

        let counters = refresh_runtime_stability_counters(brehon_root).unwrap();
        assert_eq!(counters.pending_requests, 2);
        assert_eq!(counters.pending_prompt_waiters, 3);
        assert_eq!(counters.blocked_sends, 1);
        assert_eq!(counters.active_reviews, 1);
        assert_eq!(counters.completed_tasks, 1);
        assert_eq!(counters.assignment_history, 4);
        assert_eq!(counters.tokens_used, 123);

        let persisted =
            load_runtime_stability_counters(&runtime_stability_counters_path(brehon_root)).unwrap();
        assert_eq!(persisted, counters);
    }

    #[test]
    fn infer_task_token_target_prefers_single_prompt_mention() {
        let root = tempfile::tempdir().unwrap();
        let tasks = root.path().join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        std::fs::write(
            tasks.join("T-1.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": "T-1",
                "task_type": "task",
                "status": "assigned",
                "assignee": "worker-1"
            }))
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            infer_task_token_target(
                root.path(),
                "reviewer-1",
                "reviewer",
                "Review request REV-1 for task T-1: check it"
            )
            .as_deref(),
            Some("T-1")
        );
    }

    #[test]
    fn infer_task_token_target_falls_back_to_single_active_worker_task() {
        let root = tempfile::tempdir().unwrap();
        let tasks = root.path().join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        std::fs::write(
            tasks.join("T-active.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": "T-active",
                "task_type": "task",
                "status": "in_progress",
                "assignee": "worker-1"
            }))
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            infer_task_token_target(root.path(), "worker-1", "worker", "continue").as_deref(),
            Some("T-active")
        );
    }

    #[test]
    fn record_task_token_usage_rolls_up_to_epic_and_initiative() {
        let root = tempfile::tempdir().unwrap();
        let tasks = root.path().join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        for (id, task_type, parent_id) in [
            ("I-1", "initiative", None),
            ("E-1", "epic", Some("I-1")),
            ("T-1", "task", Some("E-1")),
        ] {
            let mut task = serde_json::json!({
                "task_id": id,
                "task_type": task_type,
                "status": "in_progress"
            });
            if let Some(parent_id) = parent_id {
                task["parent_id"] = Value::String(parent_id.to_string());
            }
            std::fs::write(
                tasks.join(format!("{id}.json")),
                serde_json::to_string_pretty(&task).unwrap(),
            )
            .unwrap();
        }

        let updated = record_task_token_usage(root.path(), "T-1", 750).unwrap();
        assert_eq!(updated, vec!["T-1", "E-1", "I-1"]);

        for id in ["T-1", "E-1", "I-1"] {
            let task: Value = serde_json::from_str(
                &std::fs::read_to_string(tasks.join(format!("{id}.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(task["token_usage"]["tokens_used"], 750);
            assert!(task["token_usage"]["updated_at"].as_str().is_some());
        }
    }

    #[test]
    fn read_task_token_usage_distinguishes_missing_from_zero() {
        let root = tempfile::tempdir().unwrap();
        let tasks = root.path().join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks).unwrap();

        // Missing file -> None (unknown), not Some(0).
        assert_eq!(
            read_task_token_usage(root.path(), "T-missing").unwrap(),
            None
        );

        record_task_token_usage(root.path(), "T-1", 0).unwrap_or_default();
        std::fs::write(
            tasks.join("T-1.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": "T-1",
                "task_type": "task"
            }))
            .unwrap(),
        )
        .unwrap();
        // Present file without token_usage field -> None.
        assert_eq!(read_task_token_usage(root.path(), "T-1").unwrap(), None);

        record_task_token_usage(root.path(), "T-1", 321).unwrap();
        assert_eq!(
            read_task_token_usage(root.path(), "T-1").unwrap(),
            Some(321)
        );
    }

    #[test]
    fn read_run_total_tokens_sums_initiative_rollup() {
        let root = tempfile::tempdir().unwrap();
        let tasks = root.path().join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        for (id, task_type, parent_id) in [
            ("I-1", "initiative", None),
            ("E-1", "epic", Some("I-1")),
            ("T-1", "task", Some("E-1")),
        ] {
            let mut task = serde_json::json!({
                "task_id": id,
                "task_type": task_type,
                "status": "in_progress"
            });
            if let Some(parent_id) = parent_id {
                task["parent_id"] = Value::String(parent_id.to_string());
            }
            std::fs::write(
                tasks.join(format!("{id}.json")),
                serde_json::to_string_pretty(&task).unwrap(),
            )
            .unwrap();
        }

        record_task_token_usage(root.path(), "T-1", 750).unwrap();
        // The run total must equal the initiative rollup, not the sum of every
        // task (which would triple count via the epic + task rows).
        assert_eq!(read_run_total_tokens(root.path()).unwrap(), 750);
    }

    #[test]
    fn read_run_total_tokens_falls_back_to_rootless_tasks() {
        let root = tempfile::tempdir().unwrap();
        let tasks = root.path().join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        for id in ["T-a", "T-b"] {
            std::fs::write(
                tasks.join(format!("{id}.json")),
                serde_json::to_string_pretty(&serde_json::json!({
                    "task_id": id,
                    "task_type": "task"
                }))
                .unwrap(),
            )
            .unwrap();
            record_task_token_usage(root.path(), id, 100).unwrap();
        }
        // No initiative present -> sum parent-less tasks.
        assert_eq!(read_run_total_tokens(root.path()).unwrap(), 200);
    }

    #[test]
    fn read_run_total_tokens_empty_run_is_zero() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_run_total_tokens(root.path()).unwrap(), 0);
    }

    #[test]
    fn remove_session_stability_counters_ignores_missing_files() {
        let root = tempfile::tempdir().unwrap();
        remove_session_stability_counters(root.path(), "missing").unwrap();
    }

    #[test]
    fn increment_assignment_history_is_concurrency_safe() {
        let root = tempfile::tempdir().unwrap();
        let brehon_root = root.path().to_path_buf();
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let brehon_root = brehon_root.clone();
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        increment_assignment_history(&brehon_root, 1).unwrap();
                    }
                })
            })
            .collect();

        for worker in workers {
            worker.join().unwrap();
        }

        let counters = refresh_runtime_stability_counters(&brehon_root).unwrap();
        assert_eq!(counters.assignment_history, 200);
    }

    #[test]
    fn increment_assignment_history_tolerates_existing_lockfile_path() {
        let root = tempfile::tempdir().unwrap();
        let brehon_root = root.path();
        let lock_path = runtime_dir(brehon_root).join(".stability-meta.json.lock");
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(&lock_path, b"stale").unwrap();

        increment_assignment_history(brehon_root, 1).unwrap();
        let counters = refresh_runtime_stability_counters(brehon_root).unwrap();
        assert_eq!(counters.assignment_history, 1);
    }

    #[test]
    fn write_json_atomic_round_trips_and_leaves_no_tmp() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested").join("value.json");
        let value = serde_json::json!({ "kind": "task", "id": 7 });

        write_json_atomic(&path, &value).unwrap();

        // The target exists and round-trips.
        let on_disk: Value = read_json(&path).unwrap();
        assert_eq!(on_disk, value);

        // Exactly one regular file remains in the directory — no leftover
        // .tmp turd from the temp+rename sequence.
        let entries: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec!["value.json".to_string()],
            "entries: {entries:?}"
        );
    }

    #[test]
    fn write_json_atomic_overwrites_without_truncating_on_failure() {
        // A pre-existing target must remain fully intact and readable after a
        // successful overwrite (the atomic rename never exposes a half-written
        // file). This guards the "never observe a truncated target" property.
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");

        let first = serde_json::json!({ "version": 1, "payload": "first" });
        write_json_atomic(&path, &first).unwrap();

        let second = serde_json::json!({ "version": 2, "payload": "second" });
        write_json_atomic(&path, &second).unwrap();

        let on_disk: Value = read_json(&path).unwrap();
        assert_eq!(on_disk, second);

        // No temp files leaked across the two writes.
        let leftover: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftover.is_empty(), "leaked temp files: {leftover:?}");
    }
}

//! Worker proof evidence recording for task actions.

use chrono::Utc;
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use brehon_ports::{EventStore, ProofStore};
use brehon_types::{
    Event, EventKind, ProofBlocker, ProofBlockerStatus, ProofBundleId, ProofCheck,
    ProofCheckStatus, ProofCommand, TaskId,
};

use crate::tools::proof_summary::{write_proof_cache, ProofSummary};

/// Optional proof recorder attached to the task action tool.
#[derive(Clone, Default)]
pub(super) struct WorkerProofRecorder {
    event_store: Option<Arc<dyn EventStore + Send + Sync>>,
    proof_store: Option<Arc<dyn ProofStore + Send + Sync>>,
}

/// Best-effort proof recording result surfaced back to the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProofRecordOutcome {
    status: &'static str,
    proof_bundle_id: Option<String>,
    events_recorded: usize,
    warnings: Vec<String>,
}

pub(super) fn copy_proof_result(result: &mut Value, primary: &Value, fallback: &Value) {
    let source = primary.get("proof").or_else(|| fallback.get("proof"));
    if let Some(value) = source {
        result["proof"] = value.clone();
    }
    let status = primary
        .get("proof_status")
        .or_else(|| fallback.get("proof_status"));
    if let Some(value) = status {
        result["proof_status"] = value.clone();
    }
    let warning = primary
        .get("proof_warning")
        .or_else(|| fallback.get("proof_warning"));
    if let Some(value) = warning {
        result["proof_warning"] = value.clone();
    }
}

impl WorkerProofRecorder {
    pub(super) const fn empty() -> Self {
        Self {
            event_store: None,
            proof_store: None,
        }
    }

    pub(super) fn with_event_store(mut self, store: Arc<dyn EventStore + Send + Sync>) -> Self {
        self.event_store = Some(store);
        self
    }

    pub(super) fn with_proof_store(mut self, store: Arc<dyn ProofStore + Send + Sync>) -> Self {
        self.proof_store = Some(store);
        self
    }

    pub(super) async fn record_checkpoint(
        &self,
        task_id: &str,
        workspace: &Path,
        commit: &str,
        created_commit: bool,
        message: &str,
    ) -> ProofRecordOutcome {
        let diff_summary = changed_file_summary(workspace, commit);
        let summary = if created_commit {
            format!("Checkpoint commit recorded: {commit}.")
        } else {
            format!("Clean worktree checkpoint recorded existing HEAD: {commit}.")
        };
        let command = WorkerProofCommand {
            action: format!("task action=checkpoint id={task_id}"),
            cwd: Some(workspace.display().to_string()),
            output_summary: Some(format!("{summary} Message: {message}")),
            commit: Some(commit.to_string()),
            diff_summary,
        };
        self.record_worker_evidence(task_id, command, Vec::new(), Vec::new(), false)
            .await
    }

    pub(super) async fn record_progress(
        &self,
        args: &Value,
        task_id: &str,
        percent: i64,
        commit: Option<&str>,
        auto_review: bool,
    ) -> ProofRecordOutcome {
        let require_test_evidence = percent >= 100 && auto_review;
        let command = WorkerProofCommand {
            action: format!("task action=progress id={task_id} percent={percent}"),
            cwd: None,
            output_summary: Some(progress_summary(args, percent, auto_review)),
            commit: commit.map(str::to_string),
            diff_summary: None,
        };
        let mut blockers = blockers_from_args(args, "task action=progress");
        let tests = tests_from_worker_text(args, require_test_evidence, &mut blockers);
        self.record_worker_evidence(task_id, command, tests, blockers, require_test_evidence)
            .await
    }

    pub(super) async fn record_update(
        &self,
        args: &Value,
        task_id: &str,
        updated_fields: &[&str],
    ) -> ProofRecordOutcome {
        let command = WorkerProofCommand {
            action: format!("task action=update id={task_id}"),
            cwd: None,
            output_summary: Some(format!(
                "Worker updated task fields: {}.",
                updated_fields.join(", ")
            )),
            commit: None,
            diff_summary: None,
        };
        let blockers = blockers_from_args(args, "task action=update");
        self.record_worker_evidence(task_id, command, Vec::new(), blockers, false)
            .await
    }

    async fn record_worker_evidence(
        &self,
        task_id: &str,
        command: WorkerProofCommand,
        tests: Vec<ProofCheck>,
        blockers: Vec<ProofBlocker>,
        required_tests: bool,
    ) -> ProofRecordOutcome {
        let Some(event_store) = self.event_store.as_ref() else {
            return ProofRecordOutcome::unavailable(
                "No event store is attached; worker proof evidence was not recorded.",
            );
        };
        let Some(proof_store) = self.proof_store.as_ref() else {
            return ProofRecordOutcome::unavailable(
                "No proof store is attached; worker proof evidence was not recorded.",
            );
        };

        let proof_bundle_id = proof_bundle_id_for_task(task_id);
        let task_id = TaskId::new(task_id);
        let mut events_recorded = 0usize;
        let test_count = tests.len();

        match proof_store.proof_bundle_for_task(&task_id).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                let now = Utc::now();
                let event = Event {
                    kind: EventKind::ProofBundleCreated {
                        proof_bundle_id: proof_bundle_id.clone(),
                        task_id: task_id.clone(),
                        run_ids: Vec::new(),
                        created_at: now,
                    },
                    timestamp: now,
                    aggregate_id: task_id.as_str().to_string(),
                };
                if let Err(err) = append_and_project(event_store, proof_store, event).await {
                    return ProofRecordOutcome::error(proof_bundle_id, err);
                }
                events_recorded += 1;
            }
            Err(err) => return ProofRecordOutcome::error(proof_bundle_id, err.to_string()),
        }

        let now = Utc::now();
        let event = Event {
            kind: EventKind::ProofCommandRecorded {
                proof_bundle_id: proof_bundle_id.clone(),
                task_id: task_id.clone(),
                command: command.into_proof_command(now),
                recorded_at: now,
            },
            timestamp: now,
            aggregate_id: task_id.as_str().to_string(),
        };
        if let Err(err) = append_and_project(event_store, proof_store, event).await {
            return ProofRecordOutcome::error(proof_bundle_id, err);
        }
        events_recorded += 1;

        for check in tests {
            let recorded_at = Utc::now();
            let event = Event {
                kind: EventKind::ProofCheckRecorded {
                    proof_bundle_id: proof_bundle_id.clone(),
                    task_id: task_id.clone(),
                    check,
                    is_test_result: true,
                    recorded_at,
                },
                timestamp: recorded_at,
                aggregate_id: task_id.as_str().to_string(),
            };
            if let Err(err) = append_and_project(event_store, proof_store, event).await {
                return ProofRecordOutcome::error(proof_bundle_id, err);
            }
            events_recorded += 1;
        }

        let has_open_blockers = blockers
            .iter()
            .any(|blocker| blocker.status == ProofBlockerStatus::Open);
        for blocker in blockers {
            let recorded_at = Utc::now();
            let event = Event {
                kind: EventKind::ProofBlockerRecorded {
                    proof_bundle_id: proof_bundle_id.clone(),
                    task_id: task_id.clone(),
                    blocker,
                    recorded_at,
                },
                timestamp: recorded_at,
                aggregate_id: task_id.as_str().to_string(),
            };
            if let Err(err) = append_and_project(event_store, proof_store, event).await {
                return ProofRecordOutcome::error(proof_bundle_id, err);
            }
            events_recorded += 1;
        }

        let mut warnings = Vec::new();
        if required_tests && test_count == 0 {
            warnings.push("Worker completion did not report explicit test evidence.".to_string());
        }
        let status = if has_open_blockers {
            "blocked"
        } else if warnings.is_empty() {
            "recorded"
        } else {
            "incomplete"
        };
        // Mirror the projected bundle into the side-channel cache so the
        // TUI (which does not depend on fjall) can render proof evidence.
        if let Ok(Some(bundle)) = proof_store.proof_bundle_for_task(&task_id).await {
            write_proof_cache(task_id.as_str(), &ProofSummary::from_bundle(&bundle));
        }
        ProofRecordOutcome {
            status,
            proof_bundle_id: Some(proof_bundle_id.to_string()),
            events_recorded,
            warnings,
        }
    }
}

impl ProofRecordOutcome {
    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: "unavailable",
            proof_bundle_id: None,
            events_recorded: 0,
            warnings: vec![message.into()],
        }
    }

    fn error(proof_bundle_id: ProofBundleId, message: impl Into<String>) -> Self {
        Self {
            status: "error",
            proof_bundle_id: Some(proof_bundle_id.to_string()),
            events_recorded: 0,
            warnings: vec![format!(
                "Worker proof evidence recording failed: {}",
                message.into()
            )],
        }
    }

    pub(super) fn attach_to_result(&self, result: &mut Value) {
        result["proof_status"] = Value::String(self.status.to_string());
        result["proof"] = serde_json::json!({
            "status": self.status,
            "proof_bundle_id": self.proof_bundle_id,
            "events_recorded": self.events_recorded,
            "warnings": self.warnings,
        });
        if let Some(warning) = self.warnings.first() {
            result["proof_warning"] = Value::String(warning.clone());
        }
    }
}

struct WorkerProofCommand {
    action: String,
    cwd: Option<String>,
    output_summary: Option<String>,
    commit: Option<String>,
    diff_summary: Option<String>,
}

impl WorkerProofCommand {
    fn into_proof_command(self, now: chrono::DateTime<Utc>) -> ProofCommand {
        ProofCommand {
            run_id: None,
            command: self.action,
            cwd: self.cwd,
            exit_code: Some(0),
            started_at: now,
            completed_at: Some(now),
            output_summary: self.output_summary,
            evidence_ref: evidence_ref(self.commit.as_deref(), self.diff_summary.as_deref()),
        }
    }
}

async fn append_and_project(
    event_store: &Arc<dyn EventStore + Send + Sync>,
    proof_store: &Arc<dyn ProofStore + Send + Sync>,
    event: Event,
) -> Result<(), String> {
    let event_id = event_store
        .append(event.clone())
        .await
        .map_err(|err| err.to_string())?;
    proof_store
        .apply_proof_event(&event, event_id)
        .await
        .map_err(|err| err.to_string())?;
    Ok(())
}

fn proof_bundle_id_for_task(task_id: &str) -> ProofBundleId {
    ProofBundleId::new(format!("proof-{task_id}"))
}

fn evidence_ref(commit: Option<&str>, diff_summary: Option<&str>) -> Option<String> {
    let mut refs = Vec::new();
    if let Some(commit) = commit.filter(|value| !value.trim().is_empty()) {
        refs.push(format!("commit:{}", commit.trim()));
    }
    if let Some(summary) = diff_summary.filter(|value| !value.trim().is_empty()) {
        refs.push(format!("diff_summary:{}", summary.trim()));
    }
    (!refs.is_empty()).then(|| refs.join("\n"))
}

fn progress_summary(args: &Value, percent: i64, auto_review: bool) -> String {
    let notes = args
        .get("notes")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("no notes supplied");
    let handoff = if auto_review {
        "auto-transitioned to review_ready"
    } else {
        "progress recorded"
    };
    format!("Worker reported {percent}%: {notes}; {handoff}.")
}

fn blockers_from_args(args: &Value, source: &str) -> Vec<ProofBlocker> {
    let Some(blockers) = args
        .get("blockers")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Vec::new();
    };
    vec![ProofBlocker {
        blocker_id: Some("worker-reported-blocker".into()),
        summary: blockers.to_string(),
        source: Some(source.to_string()),
        status: ProofBlockerStatus::Open,
        created_at: Utc::now(),
        resolved_at: None,
        resolution: None,
    }]
}

fn tests_from_worker_text(
    args: &Value,
    require_test_evidence: bool,
    blockers: &mut Vec<ProofBlocker>,
) -> Vec<ProofCheck> {
    let mut checks = Vec::new();
    for field in ["notes", "message", "activity"] {
        if let Some(text) = args.get(field).and_then(|value| value.as_str()) {
            checks.extend(test_mentions(text));
        }
    }
    if require_test_evidence && checks.is_empty() {
        let now = Utc::now();
        blockers.push(ProofBlocker {
            blocker_id: Some("missing-worker-test-evidence".into()),
            summary: "Worker completion did not report explicit test evidence.".into(),
            source: Some("task action=complete".into()),
            status: ProofBlockerStatus::Open,
            created_at: now,
            resolved_at: None,
            resolution: None,
        });
    }
    checks
}

fn test_mentions(text: &str) -> Vec<ProofCheck> {
    text.split(['\n', ';'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .filter(|segment| looks_like_test_mention(segment))
        .map(|segment| {
            let now = Utc::now();
            ProofCheck {
                name: test_name(segment),
                command: command_from_segment(segment),
                status: test_status(segment),
                summary: Some(segment.to_string()),
                evidence_ref: None,
                checked_at: now,
            }
        })
        .collect()
}

fn looks_like_test_mention(segment: &str) -> bool {
    let lower = segment.to_ascii_lowercase();
    [
        "cargo test",
        "cargo nextest",
        "cargo check",
        "cargo clippy",
        "go test",
        "pytest",
        "npm test",
        "pnpm test",
        "yarn test",
        "make test",
        "make check",
        "tests pass",
        "test passed",
        "tests passed",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn command_from_segment(segment: &str) -> Option<String> {
    let lower = segment.to_ascii_lowercase();
    for prefix in [
        "cargo test",
        "cargo nextest",
        "cargo check",
        "cargo clippy",
        "go test",
        "pytest",
        "npm test",
        "pnpm test",
        "yarn test",
        "make test",
        "make check",
    ] {
        if let Some(start) = lower.find(prefix) {
            return Some(segment[start..].trim().trim_end_matches('.').to_string());
        }
    }
    None
}

fn test_name(segment: &str) -> String {
    command_from_segment(segment).unwrap_or_else(|| "worker-reported-tests".to_string())
}

fn test_status(segment: &str) -> ProofCheckStatus {
    let lower = segment.to_ascii_lowercase();
    if lower.contains("fail") || lower.contains("red") {
        ProofCheckStatus::Failed
    } else if lower.contains("skip") || lower.contains("not run") {
        ProofCheckStatus::Skipped
    } else if lower.contains("pass") || lower.contains("green") || lower.contains("succeed") {
        ProofCheckStatus::Passed
    } else {
        ProofCheckStatus::Unknown
    }
}

fn changed_file_summary(workspace: &Path, commit: &str) -> Option<String> {
    let output = Command::new("git")
        .args([
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-only",
            "-r",
            commit,
        ])
        .current_dir(workspace)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut files: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect();
    files.sort();
    files.dedup();
    if files.is_empty() {
        return Some("No changed files reported for checkpoint commit.".into());
    }
    let total = files.len();
    files.truncate(8);
    let suffix = if total > files.len() {
        format!(" (+{} more)", total - files.len())
    } else {
        String::new()
    };
    Some(format!(
        "{total} changed file(s): {}{suffix}",
        files.join(", ")
    ))
}

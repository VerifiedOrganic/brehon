//! Tests for proof bundle exposure via `get_task_context` (P5.7).

use super::{GetTaskContextTool, TaskContext};
use crate::server::ContentBlock;
use crate::tools::{Tool, TEST_ENV_LOCK};
use brehon_ports::{EventStore, ProofStore};
use brehon_store_fjall::FjallEventStore;
use brehon_types::{
    Event, EventKind, ProofBundleId, ProofCheck, ProofCheckStatus, ProofCommand, TaskId,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

struct ScopedEnv {
    vars: Vec<(String, Option<OsString>)>,
}

impl ScopedEnv {
    fn set(vars: &[(&'static str, &str)]) -> Self {
        let mut stored = Vec::with_capacity(vars.len());
        for (key, value) in vars {
            stored.push((key.to_string(), std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        Self { vars: stored }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for (key, value) in self.vars.iter().rev() {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn setup_brehon_root() -> (TempDir, PathBuf) {
    let temp = TempDir::new().unwrap();
    let brehon_root = temp.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    (temp, brehon_root)
}

fn write_task(brehon_root: &Path, task: &serde_json::Value) {
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let id = task
        .get("task_id")
        .and_then(|value| value.as_str())
        .unwrap_or("TASK-001");
    let path = tasks_dir.join(format!("{id}.json"));
    std::fs::write(path, serde_json::to_string_pretty(task).unwrap()).unwrap();
}

async fn seed_proof_bundle(store: &Arc<FjallEventStore>, task_id: &str) {
    let event_store: Arc<dyn EventStore + Send + Sync> = store.clone();
    let proof_store: Arc<dyn ProofStore + Send + Sync> = store.clone();
    let bundle_id = ProofBundleId::new(format!("proof-{task_id}"));
    let now = chrono::Utc::now();
    let create = Event {
        kind: EventKind::ProofBundleCreated {
            proof_bundle_id: bundle_id.clone(),
            task_id: TaskId::new(task_id),
            run_ids: Vec::new(),
            created_at: now,
        },
        timestamp: now,
        aggregate_id: task_id.to_string(),
    };
    let event_id = event_store.append(create.clone()).await.unwrap();
    proof_store
        .apply_proof_event(&create, event_id)
        .await
        .unwrap();

    let command = Event {
        kind: EventKind::ProofCommandRecorded {
            proof_bundle_id: bundle_id.clone(),
            task_id: TaskId::new(task_id),
            command: ProofCommand {
                run_id: None,
                command: format!("task action=progress id={task_id}"),
                cwd: None,
                exit_code: Some(0),
                started_at: now,
                completed_at: Some(now),
                output_summary: Some("Worker reported 50%".to_string()),
                evidence_ref: None,
            },
            recorded_at: now,
        },
        timestamp: now,
        aggregate_id: task_id.to_string(),
    };
    let event_id = event_store.append(command.clone()).await.unwrap();
    proof_store
        .apply_proof_event(&command, event_id)
        .await
        .unwrap();

    let check = Event {
        kind: EventKind::ProofCheckRecorded {
            proof_bundle_id: bundle_id,
            task_id: TaskId::new(task_id),
            check: ProofCheck {
                name: "cargo test".to_string(),
                command: Some("cargo test".to_string()),
                status: ProofCheckStatus::Passed,
                summary: None,
                evidence_ref: None,
                checked_at: now,
            },
            is_test_result: true,
            recorded_at: now,
        },
        timestamp: now,
        aggregate_id: task_id.to_string(),
    };
    let event_id = event_store.append(check.clone()).await.unwrap();
    proof_store
        .apply_proof_event(&check, event_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn task_context_includes_proof_summary_when_proof_store_wired() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (temp, brehon_root) = setup_brehon_root();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);
    write_task(
        &brehon_root,
        &serde_json::json!({
            "task_id": "T-proof",
            "title": "Proof exposure",
            "description": "Task that exercises proof exposure in context.",
            "status": "in_progress",
            "priority": "Medium",
            "assignee": "worker-1",
            "events": [
                {
                    "event_id": 7,
                    "kind": "TaskAssigned",
                    "timestamp": "2026-04-02T00:00:00Z",
                    "summary": "Task assigned."
                }
            ]
        }),
    );
    let store = Arc::new(FjallEventStore::new(temp.path().join("fjall")).unwrap());
    seed_proof_bundle(&store, "T-proof").await;

    let proof_store: Arc<dyn ProofStore + Send + Sync> = store.clone();
    let tool = GetTaskContextTool::new().with_proof_store(proof_store);
    let result = tool
        .execute(serde_json::json!({ "task_id": "T-proof" }))
        .await
        .unwrap();
    let ContentBlock::Text { text } = &result.content[0] else {
        panic!("expected text result");
    };
    let context: TaskContext = serde_json::from_str(text.as_str()).unwrap();
    let proof = context.proof.expect("proof summary present");
    assert!(!proof.absent);
    assert!(proof.proof_bundle_id.as_deref() == Some("proof-T-proof"));
    assert_eq!(proof.command_count, 1);
    assert_eq!(proof.test_count, 1);
    assert!(proof.commits.len() <= crate::tools::proof_summary::PROOF_SUMMARY_LIST_CAP + 1);
    assert_eq!(context.proof_event_id, Some(7));
}

#[tokio::test]
async fn task_context_flags_missing_proof_when_no_bundle_recorded() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (temp, brehon_root) = setup_brehon_root();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);
    write_task(
        &brehon_root,
        &serde_json::json!({
            "task_id": "T-proof-empty",
            "title": "Missing proof",
            "description": "no proof recorded",
            "status": "in_progress",
            "priority": "Medium",
            "assignee": "worker-1"
        }),
    );
    let store = Arc::new(FjallEventStore::new(temp.path().join("fjall")).unwrap());
    let proof_store: Arc<dyn ProofStore + Send + Sync> = store;
    let tool = GetTaskContextTool::new().with_proof_store(proof_store);
    let result = tool
        .execute(serde_json::json!({ "task_id": "T-proof-empty" }))
        .await
        .unwrap();
    let ContentBlock::Text { text } = &result.content[0] else {
        panic!("expected text result");
    };
    let context: TaskContext = serde_json::from_str(text.as_str()).unwrap();
    let proof = context
        .proof
        .expect("proof summary present even when absent");
    assert!(proof.absent);
    assert!(proof
        .missing
        .iter()
        .any(|line| line.contains("No proof bundle has been recorded")));
}

#[tokio::test]
async fn task_context_omits_proof_when_proof_store_absent() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_temp, brehon_root) = setup_brehon_root();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);
    write_task(
        &brehon_root,
        &serde_json::json!({
            "task_id": "T-proof-none",
            "title": "No proof store",
            "description": "n/a",
            "status": "in_progress",
            "priority": "Medium"
        }),
    );
    let tool = GetTaskContextTool::new();
    let result = tool
        .execute(serde_json::json!({ "task_id": "T-proof-none" }))
        .await
        .unwrap();
    let ContentBlock::Text { text } = &result.content[0] else {
        panic!("expected text result");
    };
    let context: TaskContext = serde_json::from_str(text.as_str()).unwrap();
    assert!(context.proof.is_none());
    assert!(context.proof_event_id.is_none());
}

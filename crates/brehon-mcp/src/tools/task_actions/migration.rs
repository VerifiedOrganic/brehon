//! Helpers for one-time task-schema migrations.
//!
//! Phase 4 of the integration state-machine rollout upgrades legacy
//! `integration_conflict` blobs into explicit `integration` state.

use brehon_types::is_terminal_task_status;
use serde_json::{Map, Value};

use super::integration_state::{IntegrationPhase, IntegrationState};
use super::locking::acquire_task_lock_blocking;
use super::paths::tasks_dir;
use super::persistence::{read_task, write_task};

pub(crate) fn upgrade_integration_conflict_to_integration(
    task: &Map<String, Value>,
) -> Option<IntegrationState> {
    if task.contains_key("integration") {
        return None;
    }

    // Compatibility shim for tasks written by the pre-state-machine code path.
    // The legacy blob is preserved on disk after migration so review preflight
    // (which still writes `integration_conflict` with source="review_preflight")
    // keeps working.
    //
    // Removal criterion: once review preflight migrates to the state machine
    // (or a dedicated preflight-conflict schema), delete the `integration_conflict`
    // field entirely — the `approved_integration` and bare-legacy migration paths
    // become unreachable once no production code writes the blob. Grep for
    // `integration_conflict` to find every site that still reads or writes it
    // before removing; this file, epic.rs (apply/mark/clear helpers), and
    // verification/actions.rs (the preflight writer) are the live readers.
    let legacy_conflict = task.get("integration_conflict")?;
    let legacy_conflict = legacy_conflict.as_object();

    // Source-aware migration. The legacy `integration_conflict` blob encoded
    // TWO distinct situations:
    //   - `approved_integration`: a real stuck cherry-pick that the new state
    //     machine owns. Migrate to phase=cherry_picking.
    //   - `review_preflight`: a pre-approval conflict written by the review
    //     preflight guard. The task is in changes_requested, no cherry-pick
    //     ever started — migrating to cherry_picking would leave a ghost state
    //     that forces the supervisor to run abort-integration on eventual
    //     re-integrate. Skip.
    //   - `worker_unmerged`: worker-side branch conflict, not an epic-worktree
    //     cherry-pick. Skip.
    //   - Missing source: genuine legacy data (pre-`source`-field). Migrate,
    //     since the only pre-source writer was the old integrate path.
    match string_field(legacy_conflict, "source").as_deref() {
        None | Some("approved_integration") => {}
        Some(_) => return None,
    }

    let reviewed_commits = reviewed_commits_from_legacy(legacy_conflict);
    let started_at = string_field(legacy_conflict, "recorded_at")
        .or_else(|| string_field(Some(task), "updated_at"))
        .unwrap_or_default();

    Some(IntegrationState {
        phase: IntegrationPhase::CherryPicking,
        epic_branch: string_field(legacy_conflict, "merge_target")
            .or_else(|| string_field(Some(task), "merge_target"))
            .unwrap_or_default(),
        worktree_path: String::new(),
        cherry_pick_base_head: String::new(),
        reviewed_commits,
        started_at: started_at.clone(),
        last_transition_at: started_at,
        conflicting_files: string_array_field(legacy_conflict, "conflicting_files"),
        attempts: 1,
        resolution: None,
    })
}

/// Restore `assignee` (and `review_owner` when blank) from
/// `integration_conflict.previous_worker` for non-terminal tasks whose worker
/// identity was nulled by the pre-fix conflict writer.
///
/// Returns `true` when `task` was mutated. Pure function — no I/O — so call
/// sites stay testable.
pub(crate) fn restore_nulled_assignee_for_active_conflict(task: &mut Map<String, Value>) -> bool {
    let status = task
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if is_terminal_task_status(&status) {
        return false;
    }

    let previous_worker = task
        .get("integration_conflict")
        .and_then(Value::as_object)
        .and_then(|conflict| conflict.get("previous_worker"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let Some(previous_worker) = previous_worker else {
        return false;
    };

    let mut changed = false;
    let assignee_blank = task
        .get("assignee")
        .map(|value| match value {
            Value::Null => true,
            Value::String(s) => s.trim().is_empty(),
            _ => false,
        })
        .unwrap_or(true);
    if assignee_blank {
        task.insert("assignee".into(), Value::String(previous_worker.clone()));
        changed = true;
    }

    let review_owner_blank = task
        .get("review_owner")
        .map(|value| match value {
            Value::Null => true,
            Value::String(s) => s.trim().is_empty(),
            _ => false,
        })
        .unwrap_or(true);
    if review_owner_blank {
        task.insert(
            "review_owner".into(),
            Value::String(previous_worker.clone()),
        );
        changed = true;
    }

    changed
}

/// One-shot startup migration that walks every task JSON file and applies
/// `restore_nulled_assignee_for_active_conflict`. Heals tasks deadlocked by
/// the pre-fix `apply_supervisor_integration_conflict` writer, which nulled
/// the assignee on every preflight integration conflict and stranded the
/// worker who could otherwise have called `task complete`.
pub(crate) fn restore_nulled_assignees_in_tasks_dir() {
    let Some(dir) = tasks_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json")
            || entry.file_name().to_string_lossy().starts_with('.')
        {
            continue;
        }

        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(Value::Object(mut task)) = serde_json::from_str::<Value>(&content) else {
            continue;
        };

        if !restore_nulled_assignee_for_active_conflict(&mut task) {
            continue;
        }

        let task_id = task
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "unknown".to_string());
        let restored = task
            .get("assignee")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let Ok(_lock) = acquire_task_lock_blocking(&task_id) else {
            tracing::warn!(
                task_id = task_id.as_str(),
                "failed to acquire task lock for assignee-restore migration"
            );
            continue;
        };
        if write_task(&task_id, &task) {
            tracing::info!(
                task_id = task_id.as_str(),
                restored_assignee = restored.as_str(),
                "restored assignee from integration_conflict.previous_worker"
            );
        } else {
            tracing::warn!(
                task_id = task_id.as_str(),
                "failed to persist task with restored assignee"
            );
        }
    }
}

pub(crate) fn migrate_legacy_integration_conflicts_in_tasks_dir() {
    let Some(dir) = tasks_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json")
            || entry.file_name().to_string_lossy().starts_with('.')
        {
            continue;
        }

        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(Value::Object(mut task)) = serde_json::from_str::<Value>(&content) else {
            continue;
        };

        let Some(mut integration) = upgrade_integration_conflict_to_integration(&task) else {
            continue;
        };
        if integration.cherry_pick_base_head.is_empty() {
            integration.cherry_pick_base_head = infer_cherry_pick_base_head(&task);
        }

        let task_id = task
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "unknown".to_string());
        let phase = integration.phase.as_str();
        let Ok(integration_value) = serde_json::to_value(&integration) else {
            continue;
        };
        task.insert("integration".into(), integration_value);
        let Ok(_lock) = acquire_task_lock_blocking(&task_id) else {
            tracing::warn!(
                task_id = task_id.as_str(),
                "failed to acquire task lock for migrated task"
            );
            continue;
        };
        if write_task(&task_id, &task) {
            tracing::info!(
                task_id = task_id.as_str(),
                phase,
                "migrated integration_conflict → integration"
            );
        } else {
            tracing::warn!(
                task_id = task_id.as_str(),
                "failed to persist migrated task"
            );
        }
    }
}

fn infer_cherry_pick_base_head(task: &Map<String, Value>) -> String {
    let worktree_path = task
        .get("integration_worktree")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            task.get("parent_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .and_then(read_task)
                .and_then(|parent| {
                    parent
                        .get("integration_worktree")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                })
        });

    let Some(worktree_path) = worktree_path else {
        return String::new();
    };
    let Ok(output) =
        crate::git_exec::run_git(std::path::Path::new(&worktree_path), &["rev-parse", "HEAD"])
    else {
        return String::new();
    };
    if !output.status.success() {
        return String::new();
    }

    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn reviewed_commits_from_legacy(legacy_conflict: Option<&Map<String, Value>>) -> Vec<String> {
    let mut reviewed_commits = string_array_field(legacy_conflict, "reviewed_commits");
    if reviewed_commits.is_empty() {
        if let Some(reviewed_commit) = string_field(legacy_conflict, "reviewed_commit") {
            reviewed_commits.push(reviewed_commit);
        }
    }
    reviewed_commits
}

fn string_field(map: Option<&Map<String, Value>>, key: &str) -> Option<String> {
    map.and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn string_array_field(map: Option<&Map<String, Value>>, key: &str) -> Vec<String> {
    map.and_then(|value| value.get(key))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::super::integration_state::write_integration_state;
    use super::*;
    use serde_json::json;

    fn restore_runs(input: Value) -> (bool, Map<String, Value>) {
        let mut task = input
            .as_object()
            .expect("test fixture must be object")
            .clone();
        let changed = restore_nulled_assignee_for_active_conflict(&mut task);
        (changed, task)
    }

    #[test]
    fn restore_assigns_worker_when_null_and_previous_worker_present() {
        let (changed, task) = restore_runs(json!({
            "status": "changes_requested",
            "assignee": null,
            "review_owner": null,
            "integration_conflict": {
                "owner": "supervisor",
                "previous_worker": "quick-cat-67"
            }
        }));
        assert!(changed);
        assert_eq!(task["assignee"], "quick-cat-67");
        assert_eq!(task["review_owner"], "quick-cat-67");
    }

    #[test]
    fn restore_treats_blank_string_as_null() {
        let (changed, task) = restore_runs(json!({
            "status": "in_progress",
            "assignee": "   ",
            "review_owner": "",
            "integration_conflict": {
                "owner": "supervisor",
                "previous_worker": "glad-hen-23"
            }
        }));
        assert!(changed);
        assert_eq!(task["assignee"], "glad-hen-23");
        assert_eq!(task["review_owner"], "glad-hen-23");
    }

    #[test]
    fn restore_preserves_existing_assignee() {
        // If a worker was already restored or never nulled, the migration
        // must not overwrite their identity with previous_worker.
        let (changed, task) = restore_runs(json!({
            "status": "changes_requested",
            "assignee": "different-worker",
            "review_owner": "different-worker",
            "integration_conflict": {
                "owner": "supervisor",
                "previous_worker": "quick-cat-67"
            }
        }));
        assert!(!changed);
        assert_eq!(task["assignee"], "different-worker");
        assert_eq!(task["review_owner"], "different-worker");
    }

    #[test]
    fn restore_skips_terminal_tasks() {
        // Terminal tasks (closed/merged/etc.) intentionally have null
        // assignee — `clear_terminal_task_ownership` writes that. Don't
        // un-do it just because previous_worker is parked in the conflict
        // blob.
        let (changed, task) = restore_runs(json!({
            "status": "closed",
            "assignee": null,
            "review_owner": null,
            "integration_conflict": {
                "owner": "supervisor",
                "previous_worker": "quick-cat-67"
            }
        }));
        assert!(!changed);
        assert!(task["assignee"].is_null());
        assert!(task["review_owner"].is_null());
    }

    #[test]
    fn restore_no_op_without_conflict_blob() {
        let (changed, task) = restore_runs(json!({
            "status": "pending",
            "assignee": null,
            "review_owner": null
        }));
        assert!(!changed);
        assert!(task["assignee"].is_null());
    }

    #[test]
    fn restore_no_op_when_previous_worker_missing_or_blank() {
        let (changed_missing, _) = restore_runs(json!({
            "status": "in_progress",
            "assignee": null,
            "integration_conflict": { "owner": "supervisor" }
        }));
        assert!(!changed_missing);

        let (changed_blank, _) = restore_runs(json!({
            "status": "in_progress",
            "assignee": null,
            "integration_conflict": {
                "owner": "supervisor",
                "previous_worker": "   "
            }
        }));
        assert!(!changed_blank);
    }

    #[test]
    fn restore_only_review_owner_when_assignee_already_present() {
        let (changed, task) = restore_runs(json!({
            "status": "in_progress",
            "assignee": "quick-cat-67",
            "review_owner": null,
            "integration_conflict": {
                "owner": "supervisor",
                "previous_worker": "quick-cat-67"
            }
        }));
        assert!(changed);
        assert_eq!(task["assignee"], "quick-cat-67");
        assert_eq!(task["review_owner"], "quick-cat-67");
    }

    #[test]
    fn upgrade_returns_none_when_task_already_has_integration_state() {
        let task = json!({
            "integration": {
                "phase": "cherry_picking"
            },
            "integration_conflict": {
                "merge_target": "epic/test"
            }
        });

        let upgraded = upgrade_integration_conflict_to_integration(task.as_object().unwrap());
        assert!(upgraded.is_none());
    }

    #[test]
    fn upgrade_maps_full_legacy_blob() {
        let task = json!({
            "merge_target": "epic/fallback",
            "updated_at": "2026-04-23T00:00:00Z",
            "integration_conflict": {
                "merge_target": "epic/test",
                "reviewed_commit": "deadbeef",
                "reviewed_commits": ["abc123", "deadbeef"],
                "conflicting_files": ["Cargo.toml", "src/lib.rs"],
                "recorded_at": "2026-04-22T14:32:01Z"
            }
        });

        let upgraded = upgrade_integration_conflict_to_integration(task.as_object().unwrap())
            .expect("legacy blob should migrate");

        assert_eq!(upgraded.phase, IntegrationPhase::CherryPicking);
        assert_eq!(upgraded.epic_branch, "epic/test");
        assert!(upgraded.worktree_path.is_empty());
        assert!(upgraded.cherry_pick_base_head.is_empty());
        assert_eq!(upgraded.reviewed_commits, vec!["abc123", "deadbeef"]);
        assert_eq!(upgraded.started_at, "2026-04-22T14:32:01Z");
        assert_eq!(upgraded.last_transition_at, "2026-04-22T14:32:01Z");
        assert_eq!(upgraded.conflicting_files, vec!["Cargo.toml", "src/lib.rs"]);
        assert_eq!(upgraded.attempts, 1);
        assert!(upgraded.resolution.is_none());
    }

    #[test]
    fn upgrade_maps_empty_legacy_blob_to_cherry_picking_defaults() {
        let task = json!({
            "merge_target": "epic/test",
            "updated_at": "2026-04-23T00:00:00Z",
            "integration_conflict": {}
        });

        let upgraded = upgrade_integration_conflict_to_integration(task.as_object().unwrap())
            .expect("empty legacy blob should still migrate");

        assert_eq!(upgraded.phase, IntegrationPhase::CherryPicking);
        assert_eq!(upgraded.epic_branch, "epic/test");
        assert!(upgraded.reviewed_commits.is_empty());
        assert_eq!(upgraded.started_at, "2026-04-23T00:00:00Z");
        assert_eq!(upgraded.last_transition_at, "2026-04-23T00:00:00Z");
        assert!(upgraded.conflicting_files.is_empty());
        assert_eq!(upgraded.attempts, 1);
    }

    #[test]
    fn upgrade_maps_corrupt_legacy_blob_to_safe_defaults() {
        let task = json!({
            "merge_target": "epic/test",
            "updated_at": "2026-04-23T00:00:00Z",
            "integration_conflict": {
                "merge_target": 17,
                "reviewed_commit": ["not-a-string"],
                "reviewed_commits": ["abc123", 42, "   "],
                "conflicting_files": [true, "src/main.rs", null],
                "recorded_at": false
            }
        });

        let upgraded = upgrade_integration_conflict_to_integration(task.as_object().unwrap())
            .expect("corrupt legacy blob should still produce a migration state");

        assert_eq!(upgraded.phase, IntegrationPhase::CherryPicking);
        assert_eq!(upgraded.epic_branch, "epic/test");
        assert_eq!(upgraded.reviewed_commits, vec!["abc123"]);
        assert_eq!(upgraded.started_at, "2026-04-23T00:00:00Z");
        assert_eq!(upgraded.last_transition_at, "2026-04-23T00:00:00Z");
        assert_eq!(upgraded.conflicting_files, vec!["src/main.rs"]);
        assert_eq!(upgraded.attempts, 1);
    }

    #[test]
    fn writing_migrated_state_preserves_legacy_conflict_blob_for_compatibility() {
        let task = json!({
            "task_id": "T-legacy",
            "merge_target": "epic/fallback",
            "updated_at": "2026-04-23T00:00:00Z",
            "integration_conflict": {
                "merge_target": "epic/test",
                "reviewed_commit": "deadbeef",
                "reviewed_commits": ["abc123", "deadbeef"],
                "conflicting_files": ["Cargo.toml", "src/lib.rs"],
                "recorded_at": "2026-04-22T14:32:01Z"
            }
        });

        let mut migrated_task = task.as_object().unwrap().clone();
        let upgraded = upgrade_integration_conflict_to_integration(&migrated_task)
            .expect("legacy blob should migrate");
        write_integration_state(&mut migrated_task, &upgraded);

        let persisted = Value::Object(migrated_task);
        let integration = persisted
            .get("integration")
            .and_then(Value::as_object)
            .expect("migration should write the new integration state");
        let legacy = persisted
            .get("integration_conflict")
            .and_then(Value::as_object)
            .expect("compatibility shim should keep the legacy blob");

        assert_eq!(
            integration.get("epic_branch").and_then(Value::as_str),
            legacy.get("merge_target").and_then(Value::as_str)
        );
        assert_eq!(
            integration
                .get("reviewed_commits")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            legacy
                .get("reviewed_commits")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        );
        assert_eq!(
            integration
                .get("conflicting_files")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            legacy
                .get("conflicting_files")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        );
    }

    #[test]
    fn upgrade_skips_review_preflight_source_blob() {
        // Preflight conflicts are pre-approval: task is in changes_requested and
        // no cherry-pick ever started. Migrating to phase=cherry_picking would
        // leave a ghost state forcing abort-integration on the eventual retry.
        let task = json!({
            "status": "changes_requested",
            "merge_target": "epic/test",
            "updated_at": "2026-04-23T00:00:00Z",
            "integration_conflict": {
                "owner": "supervisor",
                "source": "review_preflight",
                "merge_target": "epic/test",
                "reviewed_commit": "deadbeef",
                "reviewed_commits": ["deadbeef"],
                "conflicting_files": ["Cargo.toml"],
                "recorded_at": "2026-04-22T14:32:01Z"
            }
        });

        let upgraded = upgrade_integration_conflict_to_integration(task.as_object().unwrap());
        assert!(
            upgraded.is_none(),
            "preflight-source blobs must not be migrated into integration state"
        );
    }

    #[test]
    fn upgrade_skips_worker_unmerged_source_blob() {
        let task = json!({
            "merge_target": "epic/test",
            "updated_at": "2026-04-23T00:00:00Z",
            "integration_conflict": {
                "source": "worker_unmerged",
                "merge_target": "epic/test",
                "reviewed_commit": "deadbeef",
                "conflicting_files": ["src/lib.rs"],
                "recorded_at": "2026-04-22T14:32:01Z"
            }
        });

        let upgraded = upgrade_integration_conflict_to_integration(task.as_object().unwrap());
        assert!(
            upgraded.is_none(),
            "worker_unmerged-source blobs must not be migrated into integration state"
        );
    }

    #[test]
    fn upgrade_skips_unknown_source_blob() {
        // Defense-in-depth: an unrecognised source is treated as skip-and-preserve
        // rather than silently upgraded, since the blob's semantics are unknown.
        let task = json!({
            "merge_target": "epic/test",
            "updated_at": "2026-04-23T00:00:00Z",
            "integration_conflict": {
                "source": "some_future_source",
                "merge_target": "epic/test",
                "reviewed_commit": "deadbeef"
            }
        });

        let upgraded = upgrade_integration_conflict_to_integration(task.as_object().unwrap());
        assert!(
            upgraded.is_none(),
            "unknown-source blobs must not be migrated into integration state"
        );
    }

    #[test]
    fn upgrade_migrates_approved_integration_source_blob() {
        let task = json!({
            "merge_target": "epic/test",
            "updated_at": "2026-04-23T00:00:00Z",
            "integration_conflict": {
                "source": "approved_integration",
                "merge_target": "epic/test",
                "reviewed_commit": "deadbeef",
                "reviewed_commits": ["deadbeef"],
                "conflicting_files": ["src/lib.rs"],
                "recorded_at": "2026-04-22T14:32:01Z"
            }
        });

        let upgraded = upgrade_integration_conflict_to_integration(task.as_object().unwrap())
            .expect("approved_integration blob should migrate");
        assert_eq!(upgraded.phase, IntegrationPhase::CherryPicking);
        assert_eq!(upgraded.epic_branch, "epic/test");
    }

    #[test]
    fn phase_name_formats_phases_for_migration_logs() {
        assert_eq!(IntegrationPhase::Null.as_str(), "null");
        assert_eq!(IntegrationPhase::CherryPicking.as_str(), "cherry_picking");
        assert_eq!(IntegrationPhase::Resolved.as_str(), "resolved");
        assert_eq!(IntegrationPhase::Complete.as_str(), "complete");
        assert_eq!(IntegrationPhase::Aborted.as_str(), "aborted");
    }
}

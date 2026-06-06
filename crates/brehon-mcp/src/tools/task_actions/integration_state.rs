//! Integration state machine: explicit phases, git probes, deterministic transitions.
//!
//! Replaces the old inference-based integrate logic (SHA ancestry checks +
//! `integration_conflict` blob) with an explicit state machine stored in the
//! task JSON under the `integration` key.

use brehon_types::normalize_task_status;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Current phase of an integration attempt for a single task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationPhase {
    /// Default — no integration in progress for this task.
    #[default]
    Null,
    /// `git cherry-pick` has been started. Either succeeded (transient →
    /// Resolved) or hit a conflict (supervisor needs to act).
    CherryPicking,
    /// Reviewed commits are applied to the epic branch, verified via ancestry
    /// or patch-equivalence. Ready to finalize.
    Resolved,
    /// Task is closed, epic branch updated, nothing further.
    Complete,
    /// Operator explicitly called abort-integration or irrecoverable corruption
    /// was detected. Terminal until `force=true`.
    Aborted,
}

impl IntegrationPhase {
    /// Wire / log representation. Matches the serde rename so JSON round-trips
    /// via this string produce the same variant.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            IntegrationPhase::Null => "null",
            IntegrationPhase::CherryPicking => "cherry_picking",
            IntegrationPhase::Resolved => "resolved",
            IntegrationPhase::Complete => "complete",
            IntegrationPhase::Aborted => "aborted",
        }
    }
}

/// Durable state for a single integration attempt, stored inside task JSON.
///
/// Every field defaults so that partial JSON (e.g., a hand-edited task, or a
/// task carrying only `{ "phase": "aborted" }` from an older write path)
/// deserializes into a usable state rather than silently falling back to
/// `IntegrationState::default()` (phase `Null`) via the try-parse-or-default
/// contract in [`read_integration_state`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IntegrationState {
    #[serde(default)]
    pub phase: IntegrationPhase,
    #[serde(default)]
    pub epic_branch: String,
    #[serde(default)]
    pub worktree_path: String,
    #[serde(default)]
    pub cherry_pick_base_head: String,
    #[serde(default)]
    pub reviewed_commits: Vec<String>,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub last_transition_at: String,
    #[serde(default)]
    pub conflicting_files: Vec<String>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub resolution: Option<Resolution>,
}

impl Default for IntegrationState {
    fn default() -> Self {
        Self {
            phase: IntegrationPhase::Null,
            epic_branch: String::new(),
            worktree_path: String::new(),
            cherry_pick_base_head: String::new(),
            reviewed_commits: Vec::new(),
            started_at: String::new(),
            last_transition_at: String::new(),
            conflicting_files: Vec::new(),
            attempts: 0,
            resolution: None,
        }
    }
}

/// Record of how an integration was resolved or aborted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resolution {
    pub kind: String,
    pub reason: String,
    #[serde(alias = "aborted_at")]
    pub resolved_at: String,
}

/// Result of probing the git worktree for authoritative state.
///
/// These booleans represent the status of the full `reviewed_commits` set for
/// the current integration attempt, not just a single commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitProbeResult {
    pub cherry_pick_in_progress: bool,
    pub cherry_pick_sha: Option<String>,
    pub unmerged_files: Vec<String>,
    pub is_ancestor: bool,
    pub is_patch_equivalent: bool,
    pub has_reviewed_cherry_pick_trailers: bool,
    pub reviewed_commits_applied: bool,
    pub tree_matches_after: bool,
}

/// Action the integrate handler should execute for this transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Begin `git cherry-pick -x <commit>` in the integration worktree.
    StartCherryPick,
    /// Run `git cherry-pick --continue` (or `--skip` if empty).
    ContinueCherryPick,
    /// No-op verification path — commit already applied via ancestry or
    /// patch-equivalence.
    Verify,
    /// Close task, update refs, release resources.
    Finalize,
    /// Destructively discard the current attempt, reset to `Null`, then retry.
    ForceReset(String),
    /// Reject the transition with a human-readable reason.
    Reject(String),
    /// Return cached result without mutating git state.
    Idempotent,
}

/// Output of the pure transition function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub new_phase: IntegrationPhase,
    pub action: Action,
}

/// What the operator asked for on this integrate call.
///
/// `abort-integration` is intentionally absent: it runs through its own
/// handler (`action_abort_integration`) with bespoke I/O sequencing that
/// does not fit the pure transition-function contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorIntent {
    /// Normal integrate call.
    Integrate,
    /// Escape hatch for recovery.
    ForceIntegrate,
}

/// Read the current integration state from task JSON.
/// Returns `IntegrationState::default()` (phase `Null`) if absent or malformed.
pub(super) fn read_integration_state(task: &serde_json::Map<String, Value>) -> IntegrationState {
    task.get("integration")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

pub(super) fn validate_raw_integration_phase(
    task_id: &str,
    task: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    if let Some(raw_phase) = task
        .get("integration")
        .and_then(|value| value.get("phase"))
        .and_then(|value| value.as_str())
    {
        if !matches!(
            raw_phase,
            "null" | "cherry_picking" | "resolved" | "complete" | "aborted"
        ) {
            return Err(format!(
                "Task {task_id} has unsupported integration phase '{raw_phase}'. Expected cherry_picking, resolved, null, complete, or aborted."
            ));
        }
    }
    Ok(())
}

/// Write (or overwrite) the integration state into task JSON.
pub(super) fn write_integration_state(
    task: &mut serde_json::Map<String, Value>,
    state: &IntegrationState,
) {
    if let Ok(value) = serde_json::to_value(state) {
        task.insert("integration".into(), value);
    }
}

/// Pure transition function — fully deterministic, no I/O.
///
/// Implements the transition table from `docs/INTEGRATION_STATE_MACHINE.md`.
pub(super) fn next_state(
    current: IntegrationPhase,
    task_status: &str,
    probes: &GitProbeResult,
    reviewed_commits: &[String],
    intent: OperatorIntent,
) -> Transition {
    match current {
        IntegrationPhase::Null => null_transitions(task_status, probes, intent),
        IntegrationPhase::CherryPicking => {
            cherry_picking_transitions(probes, reviewed_commits, intent)
        }
        IntegrationPhase::Resolved => resolved_transitions(probes, intent),
        IntegrationPhase::Complete => complete_transitions(intent),
        IntegrationPhase::Aborted => aborted_transitions(probes, intent),
    }
}

fn null_transitions(
    task_status: &str,
    probes: &GitProbeResult,
    _intent: OperatorIntent,
) -> Transition {
    let normalized_status = normalize_task_status(task_status).unwrap_or(task_status);
    if normalized_status != "approved" {
        return Transition {
            new_phase: IntegrationPhase::Null,
            action: Action::Reject(format!(
                "Cannot integrate from status '{}'. Only approved subtasks can be integrated into an epic branch.",
                task_status
            )),
        };
    }
    if probes.is_ancestor || probes.is_patch_equivalent {
        Transition {
            new_phase: IntegrationPhase::Resolved,
            action: Action::Verify,
        }
    } else {
        Transition {
            new_phase: IntegrationPhase::CherryPicking,
            action: Action::StartCherryPick,
        }
    }
}

fn cherry_picking_transitions(
    probes: &GitProbeResult,
    reviewed_commits: &[String],
    intent: OperatorIntent,
) -> Transition {
    match intent {
        OperatorIntent::ForceIntegrate => {
            if let Some(reason) = cherry_picking_force_reset_reason(probes, reviewed_commits) {
                return Transition {
                    new_phase: IntegrationPhase::Null,
                    action: Action::ForceReset(reason),
                };
            }
            cherry_picking_integrate_transitions(probes, reviewed_commits)
        }
        OperatorIntent::Integrate => cherry_picking_integrate_transitions(probes, reviewed_commits),
    }
}

fn cherry_picking_integrate_transitions(
    probes: &GitProbeResult,
    reviewed_commits: &[String],
) -> Transition {
    if probes.cherry_pick_in_progress {
        if let Some(ref sha) = probes.cherry_pick_sha {
            if !reviewed_commits.contains(sha) {
                return Transition {
                    new_phase: IntegrationPhase::CherryPicking,
                    action: Action::Reject(format!(
                        "stale cherry-pick for {} blocks {}; abort-integration first",
                        sha,
                        reviewed_commits.first().map(String::as_str).unwrap_or("?")
                    )),
                };
            }
            if probes.unmerged_files.is_empty() {
                Transition {
                    new_phase: IntegrationPhase::Resolved,
                    action: Action::ContinueCherryPick,
                }
            } else {
                Transition {
                    new_phase: IntegrationPhase::CherryPicking,
                    action: Action::Reject(format!(
                        "still unresolved: {}",
                        probes.unmerged_files.join(", ")
                    )),
                }
            }
        } else {
            Transition {
                new_phase: IntegrationPhase::CherryPicking,
                action: Action::Reject(
                    "cherry-pick in progress but SHA could not be read".to_string(),
                ),
            }
        }
    } else if probes.is_ancestor
        || probes.is_patch_equivalent
        || probes.has_reviewed_cherry_pick_trailers
        || probes.reviewed_commits_applied
    {
        Transition {
            new_phase: IntegrationPhase::Resolved,
            action: Action::Verify,
        }
    } else {
        Transition {
            new_phase: IntegrationPhase::CherryPicking,
            action: Action::Reject(
                "cherry-pick was cleared but commit not applied; abort-integration or resolve"
                    .to_string(),
            ),
        }
    }
}

fn cherry_picking_force_reset_reason(
    probes: &GitProbeResult,
    reviewed_commits: &[String],
) -> Option<String> {
    if probes.cherry_pick_in_progress {
        match probes.cherry_pick_sha.as_deref() {
            Some(sha) if !reviewed_commits.iter().any(|commit| commit == sha) => Some(format!(
                "force=true discarding stale cherry-pick state for unexpected commit {sha}"
            )),
            None => Some(
                "force=true discarding cherry-pick state because CHERRY_PICK_HEAD could not be read"
                    .to_string(),
            ),
            Some(_) => None,
        }
    } else if probes.is_ancestor
        || probes.is_patch_equivalent
        || probes.has_reviewed_cherry_pick_trailers
        || probes.reviewed_commits_applied
    {
        None
    } else {
        Some(
            "force=true discarding cleared cherry-pick state because the reviewed commit set is not applied"
                .to_string(),
        )
    }
}

fn resolved_transitions(probes: &GitProbeResult, _intent: OperatorIntent) -> Transition {
    // Tree check is belt-and-suspenders. If it passes, great.
    // If not, fall back to ancestry check so the normal success path
    // isn't blocked when the cheap tree probe isn't available.
    if probes.tree_matches_after
        || probes.is_ancestor
        || probes.is_patch_equivalent
        || probes.reviewed_commits_applied
        || probes.has_reviewed_cherry_pick_trailers
    {
        Transition {
            new_phase: IntegrationPhase::Complete,
            action: Action::Finalize,
        }
    } else {
        Transition {
            new_phase: IntegrationPhase::Resolved,
            action: Action::Reject(
                "verification failed — reviewed tree does not match epic branch; abort-integration"
                    .to_string(),
            ),
        }
    }
}

fn complete_transitions(intent: OperatorIntent) -> Transition {
    match intent {
        OperatorIntent::ForceIntegrate => Transition {
            new_phase: IntegrationPhase::Complete,
            action: Action::Reject(
                "integration already completed; manual revert required before force=true retry"
                    .to_string(),
            ),
        },
        _ => Transition {
            new_phase: IntegrationPhase::Complete,
            action: Action::Idempotent,
        },
    }
}

fn aborted_transitions(probes: &GitProbeResult, intent: OperatorIntent) -> Transition {
    match intent {
        OperatorIntent::ForceIntegrate => Transition {
            new_phase: IntegrationPhase::Null,
            action: Action::ForceReset(
                "force=true discarding previously aborted integration state".to_string(),
            ),
        },
        OperatorIntent::Integrate
            if probes.is_ancestor
                || probes.is_patch_equivalent
                || probes.has_reviewed_cherry_pick_trailers
                || probes.reviewed_commits_applied =>
        {
            Transition {
                new_phase: IntegrationPhase::Complete,
                action: Action::Finalize,
            }
        }
        _ => Transition {
            new_phase: IntegrationPhase::Aborted,
            action: Action::Reject(
                "explicitly aborted and reviewed commit set is not present on merge target; use force=true to retry"
                    .to_string(),
            ),
        },
    }
}

/// Check whether a task has an active integration in a non-terminal phase.
/// Used by worker-reservation logic to know that the task is blocked on
/// supervisor action and the worker should not be considered "reserved".
pub fn task_has_active_integration(task: &serde_json::Map<String, Value>) -> bool {
    let state = read_integration_state(task);
    matches!(
        state.phase,
        IntegrationPhase::CherryPicking | IntegrationPhase::Resolved
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probes() -> GitProbeResult {
        GitProbeResult::default()
    }

    #[test]
    fn transition_null_approved_start_cherry_pick() {
        let got = next_state(
            IntegrationPhase::Null,
            "approved",
            &GitProbeResult {
                is_ancestor: false,
                is_patch_equivalent: false,
                ..probes()
            },
            &["abc123".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::CherryPicking);
        assert_eq!(got.action, Action::StartCherryPick);
    }

    #[test]
    fn transition_null_approved_verify_when_ancestor() {
        let got = next_state(
            IntegrationPhase::Null,
            "approved",
            &GitProbeResult {
                is_ancestor: true,
                is_patch_equivalent: false,
                ..probes()
            },
            &["abc123".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::Resolved);
        assert_eq!(got.action, Action::Verify);
    }

    #[test]
    fn transition_null_approved_verify_when_patch_equivalent() {
        let got = next_state(
            IntegrationPhase::Null,
            "approved",
            &GitProbeResult {
                is_ancestor: false,
                is_patch_equivalent: true,
                ..probes()
            },
            &["abc123".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::Resolved);
        assert_eq!(got.action, Action::Verify);
    }

    #[test]
    fn transition_null_non_approved_rejects() {
        for status in ["pending", "assigned", "in_progress", "changes_requested"] {
            let got = next_state(
                IntegrationPhase::Null,
                status,
                &probes(),
                &["abc123".into()],
                OperatorIntent::Integrate,
            );
            assert_eq!(got.new_phase, IntegrationPhase::Null, "status={status}");
            assert!(matches!(got.action, Action::Reject(_)), "status={status}");
        }
    }

    #[test]
    fn transition_null_normalizes_pascal_case_approved_status() {
        let got = next_state(
            IntegrationPhase::Null,
            "Approved",
            &GitProbeResult {
                is_ancestor: true,
                ..probes()
            },
            &["abc123".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::Resolved);
        assert_eq!(got.action, Action::Verify);
    }

    #[test]
    fn transition_cherry_picking_resolved_via_continue() {
        let got = next_state(
            IntegrationPhase::CherryPicking,
            "approved",
            &GitProbeResult {
                cherry_pick_in_progress: true,
                cherry_pick_sha: Some("abc123".into()),
                unmerged_files: vec![],
                ..probes()
            },
            &["abc123".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::Resolved);
        assert_eq!(got.action, Action::ContinueCherryPick);
    }

    #[test]
    fn transition_cherry_picking_still_unresolved() {
        let got = next_state(
            IntegrationPhase::CherryPicking,
            "approved",
            &GitProbeResult {
                cherry_pick_in_progress: true,
                cherry_pick_sha: Some("abc123".into()),
                unmerged_files: vec!["src/foo.rs".into()],
                ..probes()
            },
            &["abc123".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::CherryPicking);
        assert!(matches!(got.action, Action::Reject(_)));
    }

    #[test]
    fn transition_cherry_picking_stale_sha_rejects() {
        let got = next_state(
            IntegrationPhase::CherryPicking,
            "approved",
            &GitProbeResult {
                cherry_pick_in_progress: true,
                cherry_pick_sha: Some("stale00".into()),
                ..probes()
            },
            &["abc123".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::CherryPicking);
        assert!(matches!(got.action, Action::Reject(_)));
    }

    #[test]
    fn transition_cherry_picking_supervisor_resolved_out_of_band() {
        let got = next_state(
            IntegrationPhase::CherryPicking,
            "approved",
            &GitProbeResult {
                cherry_pick_in_progress: false,
                is_ancestor: true,
                ..probes()
            },
            &["abc123".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::Resolved);
        assert_eq!(got.action, Action::Verify);
    }

    #[test]
    fn transition_cherry_picking_detects_reviewed_cherry_pick_trailers() {
        let got = next_state(
            IntegrationPhase::CherryPicking,
            "approved",
            &GitProbeResult {
                cherry_pick_in_progress: false,
                has_reviewed_cherry_pick_trailers: true,
                ..probes()
            },
            &["abc123".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::Resolved);
        assert_eq!(got.action, Action::Verify);
    }

    #[test]
    fn transition_cherry_picking_detects_mixed_reviewed_commit_proofs() {
        let got = next_state(
            IntegrationPhase::CherryPicking,
            "approved",
            &GitProbeResult {
                cherry_pick_in_progress: false,
                is_patch_equivalent: false,
                has_reviewed_cherry_pick_trailers: false,
                reviewed_commits_applied: true,
                ..probes()
            },
            &["abc123".into(), "def456".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::Resolved);
        assert_eq!(got.action, Action::Verify);
    }

    #[test]
    fn transition_cherry_picking_cleared_not_applied_rejects() {
        let got = next_state(
            IntegrationPhase::CherryPicking,
            "approved",
            &GitProbeResult {
                cherry_pick_in_progress: false,
                is_ancestor: false,
                is_patch_equivalent: false,
                ..probes()
            },
            &["abc123".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::CherryPicking);
        assert!(matches!(got.action, Action::Reject(_)));
    }

    #[test]
    fn transition_resolved_to_complete() {
        let got = next_state(
            IntegrationPhase::Resolved,
            "approved",
            &GitProbeResult {
                tree_matches_after: true,
                ..probes()
            },
            &["abc123".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::Complete);
        assert_eq!(got.action, Action::Finalize);
    }

    #[test]
    fn transition_resolved_tree_fail_ancestry_ok() {
        let got = next_state(
            IntegrationPhase::Resolved,
            "approved",
            &GitProbeResult {
                tree_matches_after: false,
                is_ancestor: true,
                ..probes()
            },
            &["abc123".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::Complete);
        assert_eq!(got.action, Action::Finalize);
    }

    #[test]
    fn transition_resolved_tree_fail_patch_equivalent_ok() {
        let got = next_state(
            IntegrationPhase::Resolved,
            "approved",
            &GitProbeResult {
                tree_matches_after: false,
                is_ancestor: false,
                is_patch_equivalent: true,
                ..probes()
            },
            &["abc123".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::Complete);
        assert_eq!(got.action, Action::Finalize);
    }

    #[test]
    fn transition_resolved_tree_fail_rejects() {
        let got = next_state(
            IntegrationPhase::Resolved,
            "approved",
            &GitProbeResult {
                tree_matches_after: false,
                is_ancestor: false,
                ..probes()
            },
            &["abc123".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::Resolved);
        assert!(matches!(got.action, Action::Reject(_)));
    }

    #[test]
    fn transition_complete_idempotent() {
        let got = next_state(
            IntegrationPhase::Complete,
            "closed",
            &probes(),
            &["abc123".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::Complete);
        assert_eq!(got.action, Action::Idempotent);
    }

    #[test]
    fn transition_complete_force_rejects() {
        let got = next_state(
            IntegrationPhase::Complete,
            "closed",
            &probes(),
            &["abc123".into()],
            OperatorIntent::ForceIntegrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::Complete);
        assert!(matches!(got.action, Action::Reject(_)));
    }

    #[test]
    fn transition_aborted_rejects() {
        let got = next_state(
            IntegrationPhase::Aborted,
            "approved",
            &probes(),
            &["abc123".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::Aborted);
        assert!(matches!(got.action, Action::Reject(_)));
    }

    #[test]
    fn transition_aborted_finalizes_when_reviewed_commits_are_applied() {
        let got = next_state(
            IntegrationPhase::Aborted,
            "approved",
            &GitProbeResult {
                is_patch_equivalent: true,
                reviewed_commits_applied: true,
                ..probes()
            },
            &["abc123".into()],
            OperatorIntent::Integrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::Complete);
        assert_eq!(got.action, Action::Finalize);
    }

    #[test]
    fn transition_aborted_force_returns_null() {
        let got = next_state(
            IntegrationPhase::Aborted,
            "approved",
            &probes(),
            &["abc123".into()],
            OperatorIntent::ForceIntegrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::Null);
        assert!(matches!(got.action, Action::ForceReset(_)));
    }

    #[test]
    fn transition_cherry_picking_force_resets_irrecoverable_state() {
        let got = next_state(
            IntegrationPhase::CherryPicking,
            "approved",
            &GitProbeResult {
                cherry_pick_in_progress: false,
                is_ancestor: false,
                is_patch_equivalent: false,
                ..probes()
            },
            &["abc123".into()],
            OperatorIntent::ForceIntegrate,
        );
        assert_eq!(got.new_phase, IntegrationPhase::Null);
        assert!(matches!(got.action, Action::ForceReset(_)));
    }

    #[test]
    fn read_write_integration_state_roundtrip() {
        let mut task = serde_json::Map::new();
        let state = IntegrationState {
            phase: IntegrationPhase::CherryPicking,
            epic_branch: "epic/test".into(),
            worktree_path: "/tmp/wt".into(),
            cherry_pick_base_head: "deadbeef".into(),
            reviewed_commits: vec!["abc".into()],
            started_at: "2026-04-23T00:00:00Z".into(),
            last_transition_at: "2026-04-23T00:01:00Z".into(),
            conflicting_files: vec!["src/a.rs".into()],
            attempts: 1,
            resolution: None,
        };
        write_integration_state(&mut task, &state);
        let read_back = read_integration_state(&task);
        assert_eq!(read_back, state);
    }

    #[test]
    fn read_missing_returns_null() {
        let task = serde_json::Map::new();
        let state = read_integration_state(&task);
        assert_eq!(state.phase, IntegrationPhase::Null);
    }
}

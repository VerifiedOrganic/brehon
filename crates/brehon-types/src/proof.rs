//! Durable proof bundle types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

use crate::{ReviewId, ReviewScore, ReviewVerdict, RunId, TaskId};

/// Unique identifier for a proof bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ProofBundleId(pub String);

impl ProofBundleId {
    /// Create a new `ProofBundleId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProofBundleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for ProofBundleId {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            Err("proof bundle id cannot be empty")
        } else {
            Ok(Self::new(trimmed))
        }
    }
}

/// High-level completeness state for a proof bundle.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProofBundleStatus {
    /// Bundle exists, but required evidence is not complete yet.
    #[default]
    Incomplete,
    /// Bundle has enough evidence to support a successful outcome.
    Complete,
    /// Bundle records an unresolved blocker.
    Blocked,
    /// Bundle records a failed outcome.
    Failed,
    /// Bundle was replaced by a later run or decision.
    Superseded,
}

impl ProofBundleStatus {
    /// Return true when the bundle should not be treated as complete proof.
    pub fn is_incomplete(self) -> bool {
        matches!(self, Self::Incomplete)
    }
}

/// Durable evidence artifact for a task and its related runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProofBundle {
    /// Unique proof bundle identifier.
    pub proof_bundle_id: ProofBundleId,
    /// Task this evidence belongs to.
    pub task_id: TaskId,
    /// Runs whose evidence is included in the bundle.
    pub run_ids: Vec<RunId>,
    /// Commands observed while producing the work.
    pub commands: Vec<ProofCommand>,
    /// Non-test checks or validations recorded for the work.
    pub checks: Vec<ProofCheck>,
    /// Test command results recorded for the work.
    pub test_results: Vec<ProofCheck>,
    /// Commit identifiers or checkpoint references.
    pub commits: Vec<String>,
    /// Compact summary of changed files or diff shape.
    pub diff_summary: Option<String>,
    /// Review identifiers linked to this proof.
    pub review_ids: Vec<ReviewId>,
    /// Review scores and verdict evidence.
    pub review_scores: Vec<ProofReview>,
    /// Review finding summaries.
    pub review_findings: Vec<String>,
    /// Follow-up items raised during review.
    pub followups: Vec<String>,
    /// Integration or merge evidence, if integration ran.
    pub integration_result: Option<ProofIntegration>,
    /// Conflict summaries observed during integration or closeout.
    pub conflicts: Vec<String>,
    /// Explicit operator decisions affecting the result.
    pub operator_decisions: Vec<ProofDecision>,
    /// Explicit supervisor decisions affecting the result.
    pub supervisor_decisions: Vec<ProofDecision>,
    /// Open, resolved, or waived blockers.
    pub blockers: Vec<ProofBlocker>,
    /// Final proof status.
    pub final_status: ProofBundleStatus,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last mutation timestamp.
    pub updated_at: DateTime<Utc>,
}

impl ProofBundle {
    /// Create an empty bundle that is valid to store but visibly incomplete.
    pub fn empty(
        proof_bundle_id: ProofBundleId,
        task_id: TaskId,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            proof_bundle_id,
            task_id,
            run_ids: Vec::new(),
            commands: Vec::new(),
            checks: Vec::new(),
            test_results: Vec::new(),
            commits: Vec::new(),
            diff_summary: None,
            review_ids: Vec::new(),
            review_scores: Vec::new(),
            review_findings: Vec::new(),
            followups: Vec::new(),
            integration_result: None,
            conflicts: Vec::new(),
            operator_decisions: Vec::new(),
            supervisor_decisions: Vec::new(),
            blockers: Vec::new(),
            final_status: ProofBundleStatus::Incomplete,
            created_at,
            updated_at: created_at,
        }
    }

    /// Return true when the bundle has no recorded evidence.
    pub fn is_empty(&self) -> bool {
        self.run_ids.is_empty()
            && self.commands.is_empty()
            && self.checks.is_empty()
            && self.test_results.is_empty()
            && self.commits.is_empty()
            && self.diff_summary.is_none()
            && self.review_ids.is_empty()
            && self.review_scores.is_empty()
            && self.review_findings.is_empty()
            && self.followups.is_empty()
            && self.integration_result.is_none()
            && self.conflicts.is_empty()
            && self.operator_decisions.is_empty()
            && self.supervisor_decisions.is_empty()
            && self.blockers.is_empty()
    }

    /// Return true when the bundle still needs more evidence or has no evidence.
    pub fn is_incomplete(&self) -> bool {
        self.final_status.is_incomplete() || self.is_empty()
    }
}

/// Command evidence captured during a run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProofCommand {
    /// Run that observed this command, if known.
    pub run_id: Option<RunId>,
    /// Command line or normalized command description.
    pub command: String,
    /// Working directory, if known.
    pub cwd: Option<String>,
    /// Process exit code, if the command completed.
    pub exit_code: Option<i32>,
    /// When the command started or was observed.
    pub started_at: DateTime<Utc>,
    /// When the command completed.
    pub completed_at: Option<DateTime<Utc>>,
    /// Compact output summary.
    pub output_summary: Option<String>,
    /// Pointer to full output or artifact, if retained elsewhere.
    pub evidence_ref: Option<String>,
}

/// Result state for a proof check.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProofCheckStatus {
    /// Check passed.
    Passed,
    /// Check failed.
    Failed,
    /// Check was skipped with a reason.
    Skipped,
    /// Check result was mentioned but not verified.
    Unknown,
}

/// Validation or test evidence captured for a bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProofCheck {
    /// Human-readable check name.
    pub name: String,
    /// Command used to run the check, if command-backed.
    pub command: Option<String>,
    /// Check result.
    pub status: ProofCheckStatus,
    /// Compact result summary or failure reason.
    pub summary: Option<String>,
    /// Pointer to full output or artifact, if retained elsewhere.
    pub evidence_ref: Option<String>,
    /// When the check result was observed.
    pub checked_at: DateTime<Utc>,
}

/// Review evidence linked into a proof bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProofReview {
    /// Review identifier.
    pub review_id: ReviewId,
    /// Reviewer or panel member identifier.
    pub reviewer_id: Option<String>,
    /// Reviewer score, if supplied.
    pub score: Option<ReviewScore>,
    /// Reviewer verdict, if supplied.
    pub verdict: Option<ReviewVerdict>,
    /// Finding summaries from this review.
    pub findings: Vec<String>,
    /// Follow-up summaries from this review.
    pub followups: Vec<String>,
    /// When the review evidence was recorded.
    pub reviewed_at: DateTime<Utc>,
}

/// Integration or merge evidence linked into a proof bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProofIntegration {
    /// Integration status or result label.
    pub status: String,
    /// Source branch, if known.
    pub branch: Option<String>,
    /// Base branch, if known.
    pub base_branch: Option<String>,
    /// Worktree path, if known.
    pub worktree_path: Option<String>,
    /// Commit or merge reference produced by integration.
    pub commit: Option<String>,
    /// Integration summary.
    pub summary: Option<String>,
    /// Conflicts encountered during integration.
    pub conflicts: Vec<String>,
    /// When integration completed or was recorded.
    pub integrated_at: DateTime<Utc>,
}

/// Operator or supervisor decision evidence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProofDecision {
    /// Optional decision identifier from the source system.
    pub decision_id: Option<String>,
    /// Actor that made the decision.
    pub decided_by: String,
    /// Decision summary.
    pub decision: String,
    /// Reason supplied for the decision.
    pub reason: Option<String>,
    /// When the decision was made.
    pub decided_at: DateTime<Utc>,
}

/// Blocker evidence recorded in a proof bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProofBlocker {
    /// Optional blocker identifier from the source system.
    pub blocker_id: Option<String>,
    /// Blocker summary.
    pub summary: String,
    /// Source that reported the blocker.
    pub source: Option<String>,
    /// Whether the blocker remains open, was resolved, or was waived.
    pub status: ProofBlockerStatus,
    /// When the blocker was recorded.
    pub created_at: DateTime<Utc>,
    /// When the blocker was resolved or waived.
    pub resolved_at: Option<DateTime<Utc>>,
    /// Resolution or waiver summary.
    pub resolution: Option<String>,
}

/// Resolution state for a proof blocker.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProofBlockerStatus {
    /// Blocker is still unresolved.
    Open,
    /// Blocker was resolved.
    Resolved,
    /// Blocker was explicitly waived.
    Waived,
}

/// Cap on bullet lists rendered in compact proof summaries. Keeps consumers
/// (MCP task context, review prompts, TUI) from accidentally rendering an
/// unbounded bundle.
pub const PROOF_SUMMARY_LIST_CAP: usize = 5;

/// Compact, bounded proof bundle summary safe to embed in tool responses,
/// review prompts, and the TUI. The full `ProofBundle` projection grows as
/// commands/checks/reviews accumulate; consumers should render this summary
/// instead.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProofSummary {
    /// Proof bundle id, if a bundle exists.
    pub proof_bundle_id: Option<String>,
    /// High-level bundle status (`incomplete`, `complete`, `blocked`, ...).
    pub status: String,
    /// Number of recorded commands across all runs.
    pub command_count: usize,
    /// Number of recorded test results.
    pub test_count: usize,
    /// Number of failing tests in `test_results`.
    pub failed_tests: usize,
    /// Number of recorded non-test checks.
    pub check_count: usize,
    /// Most recent commits captured in the bundle (capped + `+N more`).
    pub commits: Vec<String>,
    /// Diff summary, if known.
    pub diff_summary: Option<String>,
    /// Review identifiers linked to this bundle (capped).
    pub reviews: Vec<String>,
    /// Reviewer verdicts captured in linked reviews (capped, lower-cased).
    pub review_verdicts: Vec<String>,
    /// Bounded list of review finding summaries.
    pub review_findings: Vec<String>,
    /// Bounded list of follow-up summaries.
    pub followups: Vec<String>,
    /// Integration status label, if integration evidence is recorded.
    pub integration_status: Option<String>,
    /// Branch the integration produced.
    pub integration_branch: Option<String>,
    /// Base/target branch for the integration.
    pub integration_base: Option<String>,
    /// Commit produced by integration, if any.
    pub integration_commit: Option<String>,
    /// Conflicts reported by integration, capped.
    pub integration_conflicts: Vec<String>,
    /// Open blocker summaries, capped.
    pub open_blockers: Vec<String>,
    /// Explicitly flagged missing or incomplete evidence.
    pub missing: Vec<String>,
    /// True when no proof bundle exists yet.
    pub absent: bool,
    /// Last update timestamp for the bundle, if any (RFC 3339).
    pub updated_at: Option<String>,
}

impl ProofSummary {
    /// Build a summary describing a missing bundle.
    pub fn absent() -> Self {
        Self {
            proof_bundle_id: None,
            status: "absent".to_string(),
            command_count: 0,
            test_count: 0,
            failed_tests: 0,
            check_count: 0,
            commits: Vec::new(),
            diff_summary: None,
            reviews: Vec::new(),
            review_verdicts: Vec::new(),
            review_findings: Vec::new(),
            followups: Vec::new(),
            integration_status: None,
            integration_branch: None,
            integration_base: None,
            integration_commit: None,
            integration_conflicts: Vec::new(),
            open_blockers: Vec::new(),
            missing: vec!["No proof bundle has been recorded for this task.".to_string()],
            absent: true,
            updated_at: None,
        }
    }

    /// Build a compact summary from a `ProofBundle` projection. Lists are
    /// truncated to `PROOF_SUMMARY_LIST_CAP` entries with a `+N more`
    /// marker so consumers cannot accidentally render the full bundle.
    pub fn from_bundle(bundle: &ProofBundle) -> Self {
        let failed_tests = bundle
            .test_results
            .iter()
            .filter(|check| check.status == ProofCheckStatus::Failed)
            .count();
        let commits = truncate_list(bundle.commits.iter().cloned());
        let reviews = truncate_list(bundle.review_ids.iter().map(|id| id.0.clone()));
        let review_verdicts = truncate_list(
            bundle
                .review_scores
                .iter()
                .filter_map(|review| review.verdict.map(verdict_str_lower).map(str::to_string)),
        );
        let review_findings = truncate_list(bundle.review_findings.iter().cloned());
        let followups = truncate_list(bundle.followups.iter().cloned());
        let integration = bundle.integration_result.as_ref();
        let integration_conflicts = integration
            .map(|integration| truncate_list(integration.conflicts.iter().cloned()))
            .unwrap_or_default();
        let open_blockers = truncate_list(
            bundle
                .blockers
                .iter()
                .filter(|blocker| matches!(blocker.status, ProofBlockerStatus::Open))
                .map(|blocker| blocker.summary.clone()),
        );

        let mut missing = Vec::new();
        if bundle.commands.is_empty() {
            missing.push("No commands recorded.".to_string());
        }
        if bundle.test_results.is_empty() {
            missing.push("No test evidence recorded.".to_string());
        }
        if bundle.review_scores.is_empty() {
            missing.push("No review evidence recorded.".to_string());
        }
        if bundle.integration_result.is_none() {
            missing.push("No integration evidence recorded.".to_string());
        }
        if failed_tests > 0 {
            missing.push(format!("{failed_tests} test result(s) recorded as failed."));
        }
        if !open_blockers.is_empty() {
            missing.push(format!("{} open blocker(s) recorded.", open_blockers.len()));
        }

        Self {
            proof_bundle_id: Some(bundle.proof_bundle_id.0.clone()),
            status: status_str(bundle.final_status).to_string(),
            command_count: bundle.commands.len(),
            test_count: bundle.test_results.len(),
            failed_tests,
            check_count: bundle.checks.len(),
            commits,
            diff_summary: bundle.diff_summary.clone(),
            reviews,
            review_verdicts,
            review_findings,
            followups,
            integration_status: integration.map(|integration| integration.status.clone()),
            integration_branch: integration.and_then(|integration| integration.branch.clone()),
            integration_base: integration.and_then(|integration| integration.base_branch.clone()),
            integration_commit: integration.and_then(|integration| integration.commit.clone()),
            integration_conflicts,
            open_blockers,
            missing,
            absent: false,
            updated_at: Some(bundle.updated_at.to_rfc3339()),
        }
    }

    /// Render the summary as a short multi-line text block suitable for
    /// review prompts and operator surfaces. Output is bounded.
    pub fn render_text(&self) -> String {
        if self.absent {
            return "Proof bundle: none recorded.".to_string();
        }
        let mut out = String::with_capacity(256);
        if let Some(id) = &self.proof_bundle_id {
            out.push_str(&format!("Proof bundle {id} ({}):\n", self.status));
        } else {
            out.push_str(&format!("Proof bundle ({}):\n", self.status));
        }
        out.push_str(&format!(
            "- commands: {}, tests: {} ({} failed), checks: {}\n",
            self.command_count, self.test_count, self.failed_tests, self.check_count
        ));
        if !self.commits.is_empty() {
            out.push_str(&format!("- commits: {}\n", self.commits.join(", ")));
        }
        if let Some(diff) = &self.diff_summary {
            out.push_str(&format!("- diff: {diff}\n"));
        }
        if !self.reviews.is_empty() {
            out.push_str(&format!(
                "- reviews: {} (verdicts: {})\n",
                self.reviews.join(", "),
                if self.review_verdicts.is_empty() {
                    "none".to_string()
                } else {
                    self.review_verdicts.join(", ")
                }
            ));
        }
        if let Some(status) = &self.integration_status {
            out.push_str(&format!("- integration: {status}"));
            if let Some(branch) = &self.integration_branch {
                out.push_str(&format!(" branch={branch}"));
            }
            if let Some(base) = &self.integration_base {
                out.push_str(&format!(" base={base}"));
            }
            if let Some(commit) = &self.integration_commit {
                out.push_str(&format!(" commit={commit}"));
            }
            out.push('\n');
            if !self.integration_conflicts.is_empty() {
                out.push_str(&format!(
                    "  conflicts: {}\n",
                    self.integration_conflicts.join(", ")
                ));
            }
        }
        if !self.open_blockers.is_empty() {
            out.push_str(&format!(
                "- open blockers: {}\n",
                self.open_blockers.join("; ")
            ));
        }
        if !self.review_findings.is_empty() {
            out.push_str(&format!(
                "- review findings: {}\n",
                self.review_findings.join("; ")
            ));
        }
        if !self.followups.is_empty() {
            out.push_str(&format!("- followups: {}\n", self.followups.join("; ")));
        }
        if !self.missing.is_empty() {
            out.push_str(&format!(
                "- missing or incomplete: {}\n",
                self.missing.join("; ")
            ));
        }
        out
    }
}

fn truncate_list<I, S>(iter: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut items: Vec<String> = iter.into_iter().map(Into::into).collect();
    if items.len() > PROOF_SUMMARY_LIST_CAP {
        let extra = items.len() - PROOF_SUMMARY_LIST_CAP;
        items.truncate(PROOF_SUMMARY_LIST_CAP);
        items.push(format!("+{extra} more"));
    }
    items
}

fn status_str(status: ProofBundleStatus) -> &'static str {
    match status {
        ProofBundleStatus::Incomplete => "incomplete",
        ProofBundleStatus::Complete => "complete",
        ProofBundleStatus::Blocked => "blocked",
        ProofBundleStatus::Failed => "failed",
        ProofBundleStatus::Superseded => "superseded",
    }
}

fn verdict_str_lower(verdict: crate::ReviewVerdict) -> &'static str {
    use crate::ReviewVerdict::*;
    match verdict {
        Approve => "approved",
        ChangesRequested => "changes_requested",
        Reject => "rejected",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_bundle_id_display_parse_and_serde() {
        let id: ProofBundleId = "proof-T-1".parse().unwrap();
        assert_eq!(id.as_str(), "proof-T-1");
        assert_eq!(id.to_string(), "proof-T-1");
        assert!("   ".parse::<ProofBundleId>().is_err());

        let json = serde_json::to_string(&id).unwrap();
        let parsed: ProofBundleId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn empty_proof_bundle_is_valid_and_visibly_incomplete() {
        let now = Utc::now();
        let bundle = ProofBundle::empty(ProofBundleId::new("proof-T-1"), TaskId::new("T-1"), now);

        assert!(bundle.is_empty());
        assert!(bundle.is_incomplete());
        assert_eq!(bundle.final_status, ProofBundleStatus::Incomplete);

        let json = serde_json::to_string(&bundle).unwrap();
        assert!(json.contains(r#""final_status":"incomplete""#));
        let parsed: ProofBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, bundle);
    }

    #[test]
    fn proof_bundle_round_trips_with_all_evidence_types() {
        let now = Utc::now();
        let review_id = ReviewId::new("review-T-1-r1");
        let run_id = RunId::new("run-T-1-worker-1");

        let bundle = ProofBundle {
            proof_bundle_id: ProofBundleId::new("proof-T-1"),
            task_id: TaskId::new("T-1"),
            run_ids: vec![run_id.clone()],
            commands: vec![ProofCommand {
                run_id: Some(run_id),
                command: "cargo test -p brehon-types proof".into(),
                cwd: Some("/repo".into()),
                exit_code: Some(0),
                started_at: now,
                completed_at: Some(now),
                output_summary: Some("proof tests passed".into()),
                evidence_ref: Some("target/proof.log".into()),
            }],
            checks: vec![ProofCheck {
                name: "dependency boundaries".into(),
                command: Some("scripts/check-dependency-boundaries.sh".into()),
                status: ProofCheckStatus::Passed,
                summary: Some("39 manifests checked".into()),
                evidence_ref: None,
                checked_at: now,
            }],
            test_results: vec![ProofCheck {
                name: "brehon-types proof".into(),
                command: Some("cargo test -p brehon-types proof".into()),
                status: ProofCheckStatus::Passed,
                summary: Some("focused proof tests passed".into()),
                evidence_ref: None,
                checked_at: now,
            }],
            commits: vec!["abc1234".into()],
            diff_summary: Some("Added proof type module".into()),
            review_ids: vec![review_id.clone()],
            review_scores: vec![ProofReview {
                review_id,
                reviewer_id: Some("reviewer-1".into()),
                score: Some(ReviewScore::new(8)),
                verdict: Some(ReviewVerdict::Approve),
                findings: vec!["No blockers".into()],
                followups: Vec::new(),
                reviewed_at: now,
            }],
            review_findings: vec!["No blockers".into()],
            followups: vec!["Track MCP exposure later".into()],
            integration_result: Some(ProofIntegration {
                status: "integrated".into(),
                branch: Some("task/T-1".into()),
                base_branch: Some("main".into()),
                worktree_path: Some("/repo/.worktrees/T-1".into()),
                commit: Some("def5678".into()),
                summary: Some("Merged cleanly".into()),
                conflicts: Vec::new(),
                integrated_at: now,
            }),
            conflicts: Vec::new(),
            operator_decisions: vec![ProofDecision {
                decision_id: Some("operator-1".into()),
                decided_by: "operator".into(),
                decision: "accept typed proof shape".into(),
                reason: Some("matches hardening plan".into()),
                decided_at: now,
            }],
            supervisor_decisions: Vec::new(),
            blockers: vec![ProofBlocker {
                blocker_id: Some("blocker-1".into()),
                summary: "No proof store yet".into(),
                source: Some("P5.1 scope".into()),
                status: ProofBlockerStatus::Waived,
                created_at: now,
                resolved_at: Some(now),
                resolution: Some("deferred to P5.3".into()),
            }],
            final_status: ProofBundleStatus::Complete,
            created_at: now,
            updated_at: now,
        };

        assert!(!bundle.is_empty());
        assert!(!bundle.is_incomplete());

        let json = serde_json::to_string(&bundle).unwrap();
        let parsed: ProofBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, bundle);
    }
}

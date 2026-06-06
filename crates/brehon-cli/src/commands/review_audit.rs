//! Offline review evidence audit for completed Brehon runs.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use brehon_types::ProofSummary;
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod args;
mod git;
#[cfg(test)]
mod tests;

pub use args::ReviewAuditArgs;
use git::GitInspector;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditBucket {
    Trusted,
    NeedsRereview,
    ManualInspect,
}

impl AuditBucket {
    fn label(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::NeedsRereview => "needs_rereview",
            Self::ManualInspect => "manual_inspect",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewAuditReport {
    pub project_root: String,
    pub brehon_root: String,
    pub target: String,
    pub generated_at: String,
    pub counts: ReviewAuditCounts,
    pub tasks: Vec<TaskReviewAudit>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReviewAuditCounts {
    pub total: usize,
    pub trusted: usize,
    pub needs_rereview: usize,
    pub manual_inspect: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskReviewAudit {
    pub task_id: String,
    pub title: Option<String>,
    pub task_status: Option<String>,
    pub completion_mode: Option<String>,
    pub integration_status: Option<String>,
    pub latest_commit: Option<String>,
    pub merged_commit: Option<String>,
    pub bucket: AuditBucket,
    pub reasons: Vec<String>,
    pub review_id: Option<String>,
    pub round: Option<u32>,
    pub review_outcome: Option<String>,
    pub threshold_result: Option<String>,
    pub threshold_reason: Option<String>,
    pub panel_source: String,
    pub expected_panel: Vec<String>,
    pub submissions: Vec<ReviewerScoreAudit>,
    pub reviewed_commits: Vec<ReviewedCommitAudit>,
    pub proof: ProofAudit,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewerScoreAudit {
    pub reviewer: String,
    pub score: Option<u8>,
    pub verdict: Option<String>,
    pub ignored_for_threshold: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewedCommitAudit {
    pub commit: String,
    pub status: ReviewedCommitStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewedCommitStatus {
    Ancestor,
    CherryPickTrailer,
    PatchEquivalent,
    Missing,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProofAudit {
    pub present: bool,
    pub status: String,
    pub proof_bundle_id: Option<String>,
    pub command_count: usize,
    pub test_count: usize,
    pub failed_tests: usize,
    pub missing: Vec<String>,
}

impl ProofAudit {
    fn missing_cache() -> Self {
        Self {
            present: false,
            status: "missing_cache".to_string(),
            proof_bundle_id: None,
            command_count: 0,
            test_count: 0,
            failed_tests: 0,
            missing: Vec::new(),
        }
    }

    fn parse_error(message: String) -> Self {
        Self {
            present: true,
            status: format!("parse_error: {message}"),
            proof_bundle_id: None,
            command_count: 0,
            test_count: 0,
            failed_tests: 0,
            missing: Vec::new(),
        }
    }

    fn from_summary(summary: ProofSummary) -> Self {
        Self {
            present: !summary.absent,
            status: summary.status,
            proof_bundle_id: summary.proof_bundle_id,
            command_count: summary.command_count,
            test_count: summary.test_count,
            failed_tests: summary.failed_tests,
            missing: summary.missing,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct TaskRecord {
    #[serde(rename = "task_id", alias = "id")]
    id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    completion_mode: Option<String>,
    #[serde(default)]
    integration_status: Option<String>,
    #[serde(default)]
    latest_commit: Option<String>,
    #[serde(default)]
    merged_commit: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReviewStateRecord {
    current_round: u32,
    current_review_id: String,
    #[serde(default)]
    panel: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReviewRequestRecord {
    review_id: String,
    #[serde(default)]
    commit: String,
    #[serde(default)]
    commits: Vec<String>,
    #[serde(default)]
    resolved_empty_commit_set: bool,
    #[serde(default)]
    reviewer_prompts: BTreeMap<String, String>,
}

impl ReviewRequestRecord {
    fn reviewed_commits(&self) -> Vec<String> {
        if self.resolved_empty_commit_set {
            Vec::new()
        } else if !self.commits.is_empty() {
            self.commits
                .iter()
                .map(|commit| commit.trim())
                .filter(|commit| !commit.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        } else {
            let commit = self.commit.trim();
            if commit.is_empty() {
                Vec::new()
            } else {
                vec![commit.to_string()]
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ConsolidatedReportRecord {
    review_id: String,
    round: u32,
    outcome: String,
    scores: Value,
    approval_count: usize,
    threshold_result: String,
    threshold_reason: String,
}

struct TaskAuditBuilder {
    task_id: String,
    title: Option<String>,
    task_status: Option<String>,
    completion_mode: Option<String>,
    integration_status: Option<String>,
    latest_commit: Option<String>,
    merged_commit: Option<String>,
    review_id: Option<String>,
    round: Option<u32>,
    review_outcome: Option<String>,
    threshold_result: Option<String>,
    threshold_reason: Option<String>,
    panel_source: String,
    expected_panel: Vec<String>,
    submissions: Vec<ReviewerScoreAudit>,
    reviewed_commits: Vec<ReviewedCommitAudit>,
    proof: ProofAudit,
    needs_rereview: Vec<String>,
    manual_inspect: Vec<String>,
}

impl TaskAuditBuilder {
    fn new(task_id: String, task: Option<TaskRecord>, proof: ProofAudit) -> Self {
        Self {
            task_id,
            title: task.as_ref().and_then(|task| task.title.clone()),
            task_status: task.as_ref().and_then(|task| task.status.clone()),
            completion_mode: task.as_ref().and_then(|task| task.completion_mode.clone()),
            integration_status: task
                .as_ref()
                .and_then(|task| task.integration_status.clone()),
            latest_commit: task.as_ref().and_then(|task| task.latest_commit.clone()),
            merged_commit: task.as_ref().and_then(|task| task.merged_commit.clone()),
            review_id: None,
            round: None,
            review_outcome: None,
            threshold_result: None,
            threshold_reason: None,
            panel_source: "unknown".to_string(),
            expected_panel: Vec::new(),
            submissions: Vec::new(),
            reviewed_commits: Vec::new(),
            proof,
            needs_rereview: Vec::new(),
            manual_inspect: Vec::new(),
        }
    }

    fn need(&mut self, reason: impl Into<String>) {
        self.needs_rereview.push(reason.into());
    }

    fn inspect(&mut self, reason: impl Into<String>) {
        self.manual_inspect.push(reason.into());
    }

    fn finish(self) -> TaskReviewAudit {
        let bucket = if !self.needs_rereview.is_empty() {
            AuditBucket::NeedsRereview
        } else if !self.manual_inspect.is_empty() {
            AuditBucket::ManualInspect
        } else {
            AuditBucket::Trusted
        };
        let mut reasons = self.needs_rereview;
        reasons.extend(self.manual_inspect);

        TaskReviewAudit {
            task_id: self.task_id,
            title: self.title,
            task_status: self.task_status,
            completion_mode: self.completion_mode,
            integration_status: self.integration_status,
            latest_commit: self.latest_commit,
            merged_commit: self.merged_commit,
            bucket,
            reasons,
            review_id: self.review_id,
            round: self.round,
            review_outcome: self.review_outcome,
            threshold_result: self.threshold_result,
            threshold_reason: self.threshold_reason,
            panel_source: self.panel_source,
            expected_panel: self.expected_panel,
            submissions: self.submissions,
            reviewed_commits: self.reviewed_commits,
            proof: self.proof,
        }
    }
}

pub fn execute(
    root: Option<&Path>,
    target: &str,
    json: bool,
    fail_on_findings: bool,
    max_target_commits: usize,
) -> Result<()> {
    let report = build_report(root, target, max_target_commits)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text_report(&report);
    }

    if fail_on_findings && (report.counts.needs_rereview > 0 || report.counts.manual_inspect > 0) {
        return Err(anyhow!(
            "review audit found {} task(s) needing re-review and {} task(s) needing manual inspection",
            report.counts.needs_rereview,
            report.counts.manual_inspect
        ));
    }

    Ok(())
}

pub(crate) fn build_report(
    root: Option<&Path>,
    target: &str,
    max_target_commits: usize,
) -> Result<ReviewAuditReport> {
    let project_root = resolve_project_root(root)?;
    let brehon_root = resolve_brehon_root(&project_root)?;
    let reviews_dir = brehon_root.join("runtime").join("reviews");
    let tasks = load_tasks(&brehon_root)?;
    let mut git = GitInspector::new(&project_root, target, max_target_commits);

    let mut task_ids = BTreeSet::new();
    if reviews_dir.exists() {
        for entry in fs::read_dir(&reviews_dir)
            .with_context(|| format!("failed to read {}", reviews_dir.display()))?
        {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                task_ids.insert(entry.file_name().to_string_lossy().to_string());
            }
        }
    }

    let mut audits = Vec::new();
    for task_id in task_ids {
        audits.push(audit_task(
            &brehon_root,
            &reviews_dir,
            &tasks,
            &mut git,
            task_id,
        ));
    }
    audits.sort_by(|left, right| left.task_id.cmp(&right.task_id));

    let mut counts = ReviewAuditCounts {
        total: audits.len(),
        ..ReviewAuditCounts::default()
    };
    for audit in &audits {
        match audit.bucket {
            AuditBucket::Trusted => counts.trusted += 1,
            AuditBucket::NeedsRereview => counts.needs_rereview += 1,
            AuditBucket::ManualInspect => counts.manual_inspect += 1,
        }
    }

    Ok(ReviewAuditReport {
        project_root: project_root.display().to_string(),
        brehon_root: brehon_root.display().to_string(),
        target: target.to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        counts,
        tasks: audits,
    })
}

fn audit_task(
    brehon_root: &Path,
    reviews_dir: &Path,
    tasks: &BTreeMap<String, TaskRecord>,
    git: &mut GitInspector,
    task_id: String,
) -> TaskReviewAudit {
    let task = tasks.get(&task_id).cloned();
    let proof = load_proof_audit(brehon_root, &task_id);
    let mut audit = TaskAuditBuilder::new(task_id.clone(), task, proof);
    let task_review_dir = reviews_dir.join(&task_id);

    let Some(round_dir) = latest_consolidated_round_dir(&task_review_dir) else {
        audit.inspect("missing_consolidated_report");
        audit.inspect_proof();
        return audit.finish();
    };

    let consolidated_path = round_dir.join("consolidated.json");
    let consolidated = match read_json::<ConsolidatedReportRecord>(&consolidated_path) {
        Ok(report) => report,
        Err(err) => {
            audit.inspect(format!("invalid_consolidated_report: {err}"));
            audit.inspect_proof();
            return audit.finish();
        }
    };

    audit.review_id = Some(consolidated.review_id.clone());
    audit.round = Some(consolidated.round);
    audit.review_outcome = Some(consolidated.outcome.clone());
    audit.threshold_result = Some(consolidated.threshold_result.clone());
    audit.threshold_reason = Some(consolidated.threshold_reason.clone());

    let request = read_json::<ReviewRequestRecord>(&round_dir.join("request.json")).ok();
    let state = read_json::<ReviewStateRecord>(&task_review_dir.join("state.json")).ok();
    let (expected_panel, panel_source) =
        expected_panel_for_round(&consolidated, request.as_ref(), state.as_ref());
    audit.expected_panel = expected_panel;
    audit.panel_source = panel_source;
    audit.submissions = score_audits(&consolidated.scores);

    audit_review_outcome(&mut audit, &consolidated, request.as_ref());
    audit_reviewed_commits(&mut audit, &consolidated, request.as_ref(), git);
    audit_task_commits(&mut audit, git);
    audit.inspect_proof();

    audit.finish()
}

impl TaskAuditBuilder {
    fn inspect_proof(&mut self) {
        if !self.is_approved_review() {
            return;
        }
        if !self.proof.present {
            self.inspect("missing_or_absent_proof_summary");
        }
        if self.proof.failed_tests > 0 {
            self.need(format!(
                "proof_records_failing_tests: {} failing",
                self.proof.failed_tests
            ));
        }
        if !self.proof.missing.is_empty() {
            self.inspect(format!(
                "proof_incomplete: {}",
                self.proof.missing.join("; ")
            ));
        }
    }

    fn is_approved_review(&self) -> bool {
        self.review_outcome.as_deref() == Some("approved")
    }
}

fn audit_review_outcome(
    audit: &mut TaskAuditBuilder,
    consolidated: &ConsolidatedReportRecord,
    request: Option<&ReviewRequestRecord>,
) {
    let approved = consolidated.outcome == "approved";
    if !approved {
        if is_terminal_or_integrated(audit) {
            audit.need(format!(
                "terminal_task_without_approved_review: outcome={}",
                consolidated.outcome
            ));
        } else {
            audit.inspect(format!("latest_review_outcome={}", consolidated.outcome));
        }
        return;
    }

    let score_reviewers: BTreeSet<String> = audit
        .submissions
        .iter()
        .map(|score| score.reviewer.clone())
        .collect();
    let expected_reviewers: BTreeSet<String> = audit.expected_panel.iter().cloned().collect();
    let mut reassignment_possible = false;

    if let Some(request) = request {
        let requested_reviewers: BTreeSet<String> =
            request.reviewer_prompts.keys().cloned().collect();
        if !requested_reviewers.is_empty() && requested_reviewers != score_reviewers {
            let missing: Vec<String> = requested_reviewers
                .difference(&score_reviewers)
                .cloned()
                .collect();
            let extra: Vec<String> = score_reviewers
                .difference(&requested_reviewers)
                .cloned()
                .collect();

            if !missing.is_empty()
                && score_reviewers.len() >= 3
                && all_reported_scores_approve(audit)
            {
                reassignment_possible = true;
                audit.inspect(format!(
                    "request_submission_mismatch_reassignment_possible: missing [{}], extra [{}]",
                    missing.join(", "),
                    extra.join(", ")
                ));
            } else if !missing.is_empty() {
                audit.need(format!(
                    "approved_with_missing_requested_reviewer(s): {}",
                    missing.join(", ")
                ));
            } else {
                audit.inspect(format!(
                    "request_submission_mismatch: extra [{}]",
                    extra.join(", ")
                ));
            }
        }
    }

    if !expected_reviewers.is_empty() {
        let missing: Vec<String> = expected_reviewers
            .difference(&score_reviewers)
            .cloned()
            .collect();
        if !missing.is_empty() && !reassignment_possible {
            audit.need(format!(
                "approved_with_missing_panel_reviewer(s): {}",
                missing.join(", ")
            ));
        }
    }

    for score in audit.submissions.clone() {
        if score.ignored_for_threshold {
            audit.need(format!(
                "approved_with_ignored_reviewer: {}",
                score.reviewer
            ));
        }
        if score.verdict.as_deref() != Some("approved") {
            audit.need(format!(
                "approved_with_non_approving_reviewer: {} verdict={}",
                score.reviewer,
                score.verdict.unwrap_or_else(|| "unknown".to_string())
            ));
        }
    }

    let expected_count = audit.expected_panel.len().max(audit.submissions.len());
    if consolidated.approval_count < expected_count && !reassignment_possible {
        audit.need(format!(
            "approval_count_below_panel_size: {}/{}",
            consolidated.approval_count, expected_count
        ));
    }
}

fn audit_reviewed_commits(
    audit: &mut TaskAuditBuilder,
    consolidated: &ConsolidatedReportRecord,
    request: Option<&ReviewRequestRecord>,
    git: &mut GitInspector,
) {
    let reviewed = request
        .map(ReviewRequestRecord::reviewed_commits)
        .unwrap_or_default();

    for commit in &reviewed {
        audit.reviewed_commits.push(git.commit_evidence(commit));
    }

    if consolidated.outcome != "approved" {
        return;
    }

    let completion_mode = audit.completion_mode.as_deref().unwrap_or("merge");
    if completion_mode == "merge" && reviewed.is_empty() {
        audit.inspect("approved_merge_task_without_reviewed_commits");
    }

    for commit in audit.reviewed_commits.clone() {
        match commit.status {
            ReviewedCommitStatus::Ancestor
            | ReviewedCommitStatus::CherryPickTrailer
            | ReviewedCommitStatus::PatchEquivalent => {}
            ReviewedCommitStatus::Missing => audit.need(format!(
                "reviewed_commit_missing_on_target: {} ({})",
                commit.commit, commit.detail
            )),
            ReviewedCommitStatus::Unknown => audit.inspect(format!(
                "reviewed_commit_not_verified: {} ({})",
                commit.commit, commit.detail
            )),
        }
    }
}

fn audit_task_commits(audit: &mut TaskAuditBuilder, git: &mut GitInspector) {
    if !audit.is_approved_review() {
        return;
    }

    let reviewed: HashSet<&str> = audit
        .reviewed_commits
        .iter()
        .map(|commit| commit.commit.as_str())
        .collect();

    if let Some(latest_commit) = audit.latest_commit.clone() {
        if !reviewed.is_empty() && !reviewed.contains(latest_commit.as_str()) {
            let equivalent = audit.reviewed_commits.iter().any(|reviewed| {
                git.commits_patch_equivalent(latest_commit.as_str(), reviewed.commit.as_str())
                    .unwrap_or(false)
            });
            if !equivalent {
                audit.need(format!("latest_commit_not_reviewed: {latest_commit}"));
            }
        }
    }

    if let Some(merged_commit) = audit.merged_commit.clone() {
        let evidence = git.commit_evidence(&merged_commit);
        match evidence.status {
            ReviewedCommitStatus::Ancestor
            | ReviewedCommitStatus::CherryPickTrailer
            | ReviewedCommitStatus::PatchEquivalent => {}
            ReviewedCommitStatus::Missing => audit.need(format!(
                "merged_commit_missing_on_target: {} ({})",
                evidence.commit, evidence.detail
            )),
            ReviewedCommitStatus::Unknown => audit.inspect(format!(
                "merged_commit_not_verified: {} ({})",
                evidence.commit, evidence.detail
            )),
        }
    }
}

fn is_terminal_or_integrated(audit: &TaskAuditBuilder) -> bool {
    matches!(
        audit.task_status.as_deref(),
        Some("closed" | "merged" | "approved")
    ) || audit.integration_status.as_deref() == Some("integrated")
}

fn all_reported_scores_approve(audit: &TaskAuditBuilder) -> bool {
    !audit.submissions.is_empty()
        && audit.submissions.iter().all(|score| {
            score.verdict.as_deref() == Some("approved") && !score.ignored_for_threshold
        })
}

fn expected_panel_for_round(
    consolidated: &ConsolidatedReportRecord,
    request: Option<&ReviewRequestRecord>,
    state: Option<&ReviewStateRecord>,
) -> (Vec<String>, String) {
    if let Some(state) = state {
        if state.current_review_id == consolidated.review_id
            && state.current_round == consolidated.round
            && !state.panel.is_empty()
        {
            let mut panel = state.panel.clone();
            panel.sort();
            panel.dedup();
            return (panel, "state".to_string());
        }
    }

    if let Some(request) = request {
        if request.review_id == consolidated.review_id && !request.reviewer_prompts.is_empty() {
            let mut panel: Vec<String> = request.reviewer_prompts.keys().cloned().collect();
            panel.sort();
            return (panel, "request".to_string());
        }
    }

    let mut panel: Vec<String> = score_audits(&consolidated.scores)
        .into_iter()
        .map(|score| score.reviewer)
        .collect();
    panel.sort();
    panel.dedup();
    (panel, "scores".to_string())
}

fn score_audits(scores: &Value) -> Vec<ReviewerScoreAudit> {
    let Some(map) = scores.as_object() else {
        return Vec::new();
    };

    let mut audits: Vec<ReviewerScoreAudit> = map
        .iter()
        .map(|(reviewer, value)| {
            let score = value
                .get("score")
                .and_then(|value| value.as_u64())
                .and_then(|value| u8::try_from(value).ok());
            let verdict = value
                .get("verdict")
                .and_then(|value| value.as_str())
                .map(normalize_verdict);
            let ignored_for_threshold = value
                .get("ignored_for_threshold")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            ReviewerScoreAudit {
                reviewer: reviewer.clone(),
                score,
                verdict,
                ignored_for_threshold,
            }
        })
        .collect();
    audits.sort_by(|left, right| left.reviewer.cmp(&right.reviewer));
    audits
}

fn normalize_verdict(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "approved" | "approve" => "approved".to_string(),
        "needs_revision" | "changes_requested" | "change_requested" => {
            "changes_requested".to_string()
        }
        "rejected" | "reject" => "rejected".to_string(),
        other => other.to_string(),
    }
}

fn latest_consolidated_round_dir(task_review_dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(task_review_dir).ok()?;
    let mut rounds = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().ok()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(round) = name
            .strip_prefix("round-")
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        if entry.path().join("consolidated.json").exists() {
            rounds.push((round, entry.path()));
        }
    }
    rounds.sort_by_key(|(round, _)| *round);
    rounds.pop().map(|(_, path)| path)
}

fn load_tasks(brehon_root: &Path) -> Result<BTreeMap<String, TaskRecord>> {
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    let mut tasks = BTreeMap::new();
    if !tasks_dir.exists() {
        return Ok(tasks);
    }

    for entry in fs::read_dir(&tasks_dir)
        .with_context(|| format!("failed to read {}", tasks_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let mut task: TaskRecord = read_json(&path)?;
        let fallback_id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_string();
        let id = task.id.clone().unwrap_or(fallback_id);
        task.id = Some(id.clone());
        tasks.insert(id, task);
    }
    Ok(tasks)
}

fn load_proof_audit(brehon_root: &Path, task_id: &str) -> ProofAudit {
    let path = brehon_root
        .join("runtime")
        .join("proof")
        .join(format!("{task_id}.json"));
    if !path.exists() {
        return ProofAudit::missing_cache();
    }
    match read_json::<ProofSummary>(&path) {
        Ok(summary) => ProofAudit::from_summary(summary),
        Err(err) => ProofAudit::parse_error(err.to_string()),
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

fn resolve_project_root(root: Option<&Path>) -> Result<PathBuf> {
    let candidate = root
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let candidate = fs::canonicalize(&candidate)
        .with_context(|| format!("failed to resolve {}", candidate.display()))?;

    if candidate.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
        return candidate
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("{} has no parent project root", candidate.display()));
    }
    Ok(candidate)
}

fn resolve_brehon_root(project_root: &Path) -> Result<PathBuf> {
    if project_root.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
        return Ok(project_root.to_path_buf());
    }
    let brehon_root = project_root.join(".brehon");
    if !brehon_root.exists() {
        return Err(anyhow!(
            "Brehon runtime root not found at {}",
            brehon_root.display()
        ));
    }
    Ok(brehon_root)
}

fn print_text_report(report: &ReviewAuditReport) {
    println!(
        "Review audit: {} (target {})",
        report.project_root, report.target
    );
    println!(
        "trusted={} needs_rereview={} manual_inspect={} total={}",
        report.counts.trusted,
        report.counts.needs_rereview,
        report.counts.manual_inspect,
        report.counts.total
    );

    print_bucket(report, AuditBucket::NeedsRereview, "Needs re-review");
    print_bucket(report, AuditBucket::ManualInspect, "Manual inspect");

    if report.counts.needs_rereview == 0 && report.counts.manual_inspect == 0 {
        println!("All audited review tasks are trusted by available evidence.");
    }
}

fn print_bucket(report: &ReviewAuditReport, bucket: AuditBucket, title: &str) {
    let tasks: Vec<&TaskReviewAudit> = report
        .tasks
        .iter()
        .filter(|task| task.bucket == bucket)
        .collect();
    if tasks.is_empty() {
        return;
    }

    println!();
    println!("{title}:");
    for task in tasks {
        let review = task.review_id.as_deref().unwrap_or("no-review");
        let round = task
            .round
            .map(|round| round.to_string())
            .unwrap_or_else(|| "?".to_string());
        println!(
            "  {} round {} {} [{}]",
            task.task_id,
            round,
            review,
            bucket.label()
        );
        for reason in &task.reasons {
            println!("    - {reason}");
        }
    }
}

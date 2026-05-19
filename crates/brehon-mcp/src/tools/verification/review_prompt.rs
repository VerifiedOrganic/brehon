//! Builder for the per-reviewer prompt sent on `verification action=request_review`.
//!
//! Lives next to `scoring.rs`'s feedback builders. The single call site is
//! `actions::handle_request_review`. Extracted so:
//!
//! 1. The prompt content is unit-testable as a pure function.
//! 2. Adding a new field (e.g. base commit, source branch) is a struct-field
//!    edit + one block, not a hand-edited format string in the dispatch path.
//! 3. The "shared object database, don't cd" guidance has one home and cannot
//!    rot independently of the prompt that surrounds it.
//!
//! See `brehon-mux/src/teams.rs` for the transport that delivers this string;
//! this module is delivery-agnostic.

use serde_json::Value;

use crate::tools::proof_summary::ProofSummary;

/// Fields needed to render a review request prompt for one reviewer.
///
/// All `Option` fields gracefully degrade — when `None` the corresponding
/// guidance line is omitted rather than rendered with a placeholder.
#[derive(Debug)]
pub(crate) struct ReviewRequestPromptInput<'a> {
    pub review_id: &'a str,
    pub task_id: &'a str,
    pub title: &'a str,
    pub description: &'a str,
    pub context: &'a str,
    pub panel_id: &'a str,
    pub round: u32,
    pub reviewer: &'a str,

    /// SHA being reviewed. Empty string if absent (close-mode tasks).
    pub commit: &'a str,
    /// Commit the worker branched from. Used to suggest a useful diff range.
    pub base_commit: Option<&'a str>,
    /// Branch the worker pushed `commit` on. From `task.branch`.
    pub worker_branch: Option<&'a str>,
    /// Branch this work will merge into. From `task.merge_target` or detected default.
    pub merge_target: Option<&'a str>,
    /// Stable review scope metadata persisted with the round request.
    pub review_fingerprint: Option<&'a Value>,
    /// Compact proof bundle summary the reviewer can consult before reviewing.
    /// Present even when no bundle exists yet so missing evidence is visible.
    pub proof_summary: Option<&'a ProofSummary>,
    /// Compact research artifacts attached to the task. Advisory only; the
    /// reviewer must verify claims against the reviewed commit.
    pub research_context: Option<&'a str>,
}

/// Render the review-request prompt for one reviewer.
///
/// Output is a single string ready to hand to `notify_agent`. Formatting is
/// stable enough to assert on in tests; specific guidance lines are tested by
/// substring match rather than full-string equality so wording can evolve.
pub(crate) fn build_review_request_prompt(input: &ReviewRequestPromptInput<'_>) -> String {
    let ReviewRequestPromptInput {
        review_id,
        task_id,
        title,
        description,
        context,
        panel_id,
        round,
        reviewer,
        commit,
        base_commit,
        worker_branch,
        merge_target,
        review_fingerprint,
        proof_summary,
        research_context,
    } = input;

    let mut out = String::with_capacity(1024);

    // Header
    out.push_str(&format!(
        "Review request {review_id} for task {task_id}: {title}\n",
    ));
    out.push_str(&format!("Panel: {panel_id}\n"));
    out.push_str(&format!("Round: {round}\n"));

    if !description.is_empty() {
        out.push_str(&format!("Description: {description}\n"));
    }

    // Source line — only when we know the worker's branch. Pairs with the
    // commit access block below; together they answer "where is this work?"
    // without the reviewer having to guess.
    if let Some(branch) = worker_branch {
        match merge_target {
            Some(target) => out.push_str(&format!(
                "Source: branch {branch} (will merge into {target})\n"
            )),
            None => out.push_str(&format!("Source: branch {branch}\n")),
        }
    }

    if !commit.is_empty() {
        out.push_str(&format!("Commit: {commit}\n"));
    }
    if let Some(base) = base_commit {
        out.push_str(&format!("Base: {base}\n"));
    }
    if let Some(fingerprint) = review_fingerprint {
        append_review_fingerprint(&mut out, fingerprint);
    }
    if !context.is_empty() {
        out.push_str("\nReview handoff context:\n");
        out.push_str(context.trim());
        out.push('\n');
        out.push_str(
            "\nHandoff context is historical context, not evidence. \
             Re-verify every prior finding against the exact commit below. \
             Do not repeat a prior finding unless it is visible in this commit's diff.\n",
        );
    }

    if let Some(research) = research_context
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        out.push_str("\nResearch context:\n");
        out.push_str(research);
        out.push('\n');
    }

    // Proof bundle digest — recorded evidence of work so far. Designed to
    // help reviewers see what was already exercised (commands, tests,
    // commits, integration) without rerunning everything, and to make
    // missing evidence visible. The digest never replaces the review gate:
    // reviewers must still verify the commit diff.
    if let Some(proof) = proof_summary {
        out.push_str("\nRecorded proof of work so far:\n");
        out.push_str(&indent_block(&proof.render_text(), "  "));
        if !proof.absent {
            if !proof.missing.is_empty() {
                out.push_str("\nMissing or incomplete evidence reported by the proof bundle:\n");
                for note in &proof.missing {
                    out.push_str(&format!("- {note}\n"));
                }
            }
        }
        out.push_str(
            "\nThe proof bundle is recorded evidence, not approval. \
             Verify every claim against the commit diff before assigning a verdict, \
             and do not skip review steps because they appear in the bundle.\n",
        );
    }

    // Inspecting the commit — the structural addition. Closes the gap that
    // produced the failure mode where a reviewer with a SHA but no path
    // hint cd's to the wrong directory and thrashes for several commands.
    //
    // Invariant: every Brehon worktree shares one .git object database, so
    // any commit is reachable from any worktree by SHA without changing
    // directories. This block tells the reviewer that explicitly.
    if !commit.is_empty() {
        out.push_str("\nInspecting the commit:\n");
        out.push_str(
            "- All Brehon worktrees share one .git object database. \
             The commit is reachable from your current worktree by SHA.\n",
        );
        out.push_str(&format!("- git show {commit} --stat\n"));
        out.push_str(&format!("- git show {commit}\n"));
        let diff_range = match (base_commit, merge_target) {
            (Some(base), _) => format!("git diff {base}..{commit}"),
            (None, Some(target)) => format!("git diff {target}..{commit}"),
            (None, None) => format!("git show {commit}"),
        };
        out.push_str(&format!("- {diff_range}\n"));
        out.push_str(&format!("- git log {commit} -1\n"));
        out.push_str(
            "- Do NOT cd to other worktrees, the main repository, or your home directory. \
             The shared object database makes the commit visible from where you already are.\n",
        );
        out.push_str(
            "- Do NOT review local staged, unstaged, or uncommitted working-tree changes. \
             They may belong to an older round or another task. The review source of truth is \
             the commit above and the diff commands above.\n",
        );
        out.push_str(
            "- If prior feedback mentions staged or uncommitted changes, treat that feedback as \
             stale unless you can reproduce the issue from this commit's diff.\n",
        );
    }

    // Path interpretation — narrowed to file paths in findings, separated
    // from commit access so the two are no longer conflated.
    out.push_str("\nPath interpretation:\n");
    out.push_str(
        "Paths: treat all file paths as repository-relative to your current worktree root.\n",
    );
    out.push_str(
        "Do not reinterpret them as another agent's checkout or as absolute host paths.\n",
    );

    out.push_str("\nReview for: correctness, security, performance, concurrency, error handling, and maintainability.\n");
    out.push_str(
        "Use the handoff context, commit diff, and listed file/test hints as the starting scope. \
         Expand beyond that only when imports, call sites, tests, or the diff create a concrete risk.\n",
    );
    out.push_str(
        "Be strict about review debt: do not waive, dismiss, or summarize away legitimate nitpicks. \
         Submit every real nitpick as a structured finding with severity `nitpick`.\n",
    );
    out.push_str(
        "Only omit a nitpick when it is demonstrably false, duplicate, or outside the requested diff, \
         and mention that reasoning in the summary. Treat missing or insufficient tests as a real gap \
         unless explicitly waived.\n",
    );

    out.push_str(&format!(
        "\nSubmit your review (IMPORTANT: include reviewer={reviewer}):\n  \
         verification action=submit_review review_id={review_id} \
         reviewer={reviewer} score=<1-10> verdict=<approved|needs_revision|rejected> \
         summary=\"Your review\" findings='[{{\"description\":\"...\", \
         \"file\":\"...\", \"line\":42, \"severity\":\"blocking|suggestion|nitpick\", \
         \"suggestion\":\"optional\"}}]'\n",
    ));
    out.push_str(
        "\nDo not call request_review, reseat_panel, reassign_panel, release_panel, reset_rounds, \
         or override. Those are supervisor/maintenance actions, not reviewer actions.",
    );

    out.push_str(
        "\nScore meanings: 1-3=reject, 4-5=blocking changes, 6=real uncertainty or insufficient verification, \
         7=acceptable with all non-blocking issues captured, 8-9=good with only minor captured issues, \
         10=clean with no findings.",
    );

    out
}

fn indent_block(text: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.matches('\n').count() * prefix.len());
    for line in text.lines() {
        out.push_str(prefix);
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn fingerprint_str<'a>(fingerprint: &'a Value, key: &str) -> Option<&'a str> {
    fingerprint
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn append_review_fingerprint(out: &mut String, fingerprint: &Value) {
    if !fingerprint.is_object() {
        return;
    }
    out.push_str("Review fingerprint:\n");
    if let Some(round) = fingerprint
        .get("review_round")
        .and_then(|value| value.as_u64())
    {
        out.push_str(&format!("- round: {round}\n"));
    }
    if let Some(commit) = fingerprint_str(fingerprint, "review_commit") {
        out.push_str(&format!("- review_commit: {commit}\n"));
    }
    if let Some(base) = fingerprint_str(fingerprint, "base_commit") {
        out.push_str(&format!("- base_commit: {base}\n"));
    }
    if let Some(target_head) = fingerprint_str(fingerprint, "merge_target_head") {
        out.push_str(&format!("- merge_target_head: {target_head}\n"));
    }
    if let Some(count) = fingerprint
        .get("reviewed_commit_count")
        .and_then(|value| value.as_u64())
    {
        out.push_str(&format!("- reviewed_commit_count: {count}\n"));
    }
    if let Some(count) = fingerprint
        .get("diff_file_count")
        .and_then(|value| value.as_u64())
    {
        out.push_str(&format!("- diff_file_count: {count}\n"));
    }
    if let Some(hash) = fingerprint_str(fingerprint, "diff_stat_hash") {
        out.push_str(&format!("- diff_stat_hash: {hash}\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input<'a>() -> ReviewRequestPromptInput<'a> {
        ReviewRequestPromptInput {
            review_id: "REV-abc123",
            task_id: "T-deadbeef",
            title: "Add metrics endpoint",
            description: "Adds /metrics route with prometheus exposition.",
            context: "",
            panel_id: "panel-xyz",
            round: 1,
            reviewer: "bold-jay-79",
            commit: "e038ab51ea1f363c2449862cc438b9f823954df1",
            base_commit: None,
            worker_branch: None,
            merge_target: None,
            review_fingerprint: None,
            proof_summary: None,
            research_context: None,
        }
    }

    #[test]
    fn header_has_review_id_task_id_panel_round_and_reviewer_personalization() {
        let prompt = build_review_request_prompt(&base_input());
        assert!(
            prompt.contains("Review request REV-abc123 for task T-deadbeef: Add metrics endpoint")
        );
        assert!(prompt.contains("Panel: panel-xyz"));
        assert!(prompt.contains("Round: 1"));
        assert!(prompt.contains("reviewer=bold-jay-79"));
    }

    #[test]
    fn states_shared_object_db_invariant() {
        let prompt = build_review_request_prompt(&base_input());
        assert!(
            prompt.contains("All Brehon worktrees share one .git object database"),
            "prompt must teach the shared-object-DB invariant or reviewers thrash"
        );
    }

    #[test]
    fn names_the_anti_pattern_explicitly() {
        let prompt = build_review_request_prompt(&base_input());
        assert!(
            prompt.contains("Do NOT cd to other worktrees"),
            "prompt must explicitly disallow cd; the failure mode in the wild was cd to home dir"
        );
    }

    #[test]
    fn includes_concrete_git_commands_using_the_real_sha() {
        let prompt = build_review_request_prompt(&base_input());
        let sha = "e038ab51ea1f363c2449862cc438b9f823954df1";
        assert!(prompt.contains(&format!("git show {sha} --stat")));
        assert!(prompt.contains(&format!("git show {sha}\n")));
        assert!(prompt.contains(&format!("git log {sha} -1")));
    }

    #[test]
    fn requires_structured_nitpick_findings_instead_of_waiving_them() {
        let prompt = build_review_request_prompt(&base_input());

        assert!(prompt.contains("do not waive, dismiss, or summarize away legitimate nitpicks"));
        assert!(prompt.contains("severity `nitpick`"));
        assert!(prompt.contains("missing or insufficient tests as a real gap"));
        assert!(prompt.contains("10=clean with no findings"));
    }

    #[test]
    fn exact_commit_is_authoritative_over_local_staged_state() {
        let prompt = build_review_request_prompt(&base_input());

        assert!(prompt.contains("Do NOT review local staged, unstaged, or uncommitted"));
        assert!(prompt.contains("The review source of truth is"));
        assert!(prompt.contains("the commit above"));
    }

    #[test]
    fn prior_feedback_must_be_reverified_against_current_commit() {
        let mut input = base_input();
        input.context = "Previous review feedback to verify:\n- staged changes removed the fix";
        let prompt = build_review_request_prompt(&input);

        assert!(prompt.contains("Handoff context is historical context, not evidence"));
        assert!(prompt.contains("Do not repeat a prior finding unless it is visible"));
        assert!(prompt.contains("treat that feedback as stale"));
    }

    #[test]
    fn diff_range_uses_base_commit_when_known() {
        let mut input = base_input();
        let base = "abc123def456";
        input.base_commit = Some(base);
        let prompt = build_review_request_prompt(&input);
        assert!(
            prompt.contains(&format!("git diff {base}..{}", input.commit)),
            "base commit must be the preferred diff anchor"
        );
    }

    #[test]
    fn diff_range_falls_back_to_merge_target_when_base_unknown() {
        let mut input = base_input();
        input.merge_target = Some("main");
        let prompt = build_review_request_prompt(&input);
        assert!(
            prompt.contains(&format!("git diff main..{}", input.commit)),
            "merge target is the second-best diff anchor when base commit is missing"
        );
    }

    #[test]
    fn source_line_appears_when_worker_branch_known() {
        let mut input = base_input();
        input.worker_branch = Some("epic/phase-1-ttl-expiry-d0057eff");
        input.merge_target = Some("main");
        let prompt = build_review_request_prompt(&input);
        assert!(prompt
            .contains("Source: branch epic/phase-1-ttl-expiry-d0057eff (will merge into main)"));
    }

    #[test]
    fn source_line_omits_merge_target_when_unknown() {
        let mut input = base_input();
        input.worker_branch = Some("feature/x");
        let prompt = build_review_request_prompt(&input);
        assert!(prompt.contains("Source: branch feature/x\n"));
        assert!(!prompt.contains("will merge into"));
    }

    #[test]
    fn source_line_omitted_entirely_when_branch_unknown() {
        let prompt = build_review_request_prompt(&base_input());
        assert!(!prompt.contains("Source:"));
    }

    #[test]
    fn includes_review_fingerprint_when_available() {
        let mut input = base_input();
        let fingerprint = serde_json::json!({
            "review_round": 7,
            "review_commit": input.commit,
            "base_commit": "abc123",
            "merge_target_head": "def456",
            "reviewed_commit_count": 2,
            "diff_file_count": 4,
            "diff_stat_hash": "fnv1a64:1234abcd"
        });
        input.review_fingerprint = Some(&fingerprint);
        let prompt = build_review_request_prompt(&input);

        assert!(prompt.contains("Review fingerprint:"));
        assert!(prompt.contains("- round: 7"));
        assert!(prompt.contains("- diff_file_count: 4"));
        assert!(prompt.contains("- diff_stat_hash: fnv1a64:1234abcd"));
    }

    #[test]
    fn description_omitted_when_empty() {
        let mut input = base_input();
        input.description = "";
        let prompt = build_review_request_prompt(&input);
        assert!(!prompt.contains("Description:"));
    }

    #[test]
    fn context_included_only_when_present() {
        let mut input = base_input();
        let prompt = build_review_request_prompt(&input);
        assert!(!prompt.contains("Context:"));

        input.context = "The reaper interval is configurable via env.";
        let prompt = build_review_request_prompt(&input);
        assert!(prompt.contains("Review handoff context:"));
        assert!(prompt.contains("The reaper interval is configurable via env."));
    }

    #[test]
    fn review_scope_guidance_discourages_unbounded_search() {
        let prompt = build_review_request_prompt(&base_input());
        assert!(prompt.contains("listed file/test hints as the starting scope"));
        assert!(prompt.contains("Expand beyond that only when"));
    }

    #[test]
    fn inspecting_block_omitted_when_no_commit() {
        let mut input = base_input();
        input.commit = "";
        let prompt = build_review_request_prompt(&input);
        assert!(!prompt.contains("Inspecting the commit"));
        assert!(!prompt.contains("git show"));
    }

    #[test]
    fn path_interpretation_block_independent_of_commit_access_block() {
        // The original prompt smashed these together so reviewers read the
        // "Paths" line as advice about commit lookup. They must be visibly
        // separate sections.
        let prompt = build_review_request_prompt(&base_input());
        let inspect = prompt.find("Inspecting the commit:").unwrap();
        let paths = prompt.find("Path interpretation:").unwrap();
        assert!(
            inspect < paths,
            "Inspecting block must precede Path interpretation block"
        );
        let between = &prompt[inspect..paths];
        assert!(
            between.contains("\n\n"),
            "Sections must be separated by a blank line"
        );
    }

    #[test]
    fn submission_contract_preserved() {
        let prompt = build_review_request_prompt(&base_input());
        assert!(prompt.contains("verification action=submit_review review_id=REV-abc123"));
        assert!(prompt.contains("score=<1-10>"));
        assert!(prompt.contains("verdict=<approved|needs_revision|rejected>"));
        assert!(prompt.contains("severity"));
    }

    #[test]
    fn score_legend_preserved() {
        let prompt = build_review_request_prompt(&base_input());
        assert!(prompt.contains("1-3=reject"));
        assert!(prompt.contains("10=clean with no findings"));
    }

    // ── Proof bundle inclusion (P5.8) ──────────────────────────────────────

    fn proof_summary_recorded() -> ProofSummary {
        use brehon_types::{
            ProofBlocker, ProofBlockerStatus, ProofBundleId, ProofCheck, ProofCheckStatus,
            ProofCommand, TaskId,
        };
        let now = chrono::Utc::now();
        let mut bundle = brehon_types::ProofBundle::empty(
            ProofBundleId::new("proof-T-deadbeef"),
            TaskId::new("T-deadbeef"),
            now,
        );
        bundle.commands.push(ProofCommand {
            run_id: None,
            command: "task action=progress id=T-deadbeef percent=100".to_string(),
            cwd: None,
            exit_code: Some(0),
            started_at: now,
            completed_at: Some(now),
            output_summary: Some("Worker reported 100%".to_string()),
            evidence_ref: None,
        });
        bundle.test_results.push(ProofCheck {
            name: "cargo test".to_string(),
            command: Some("cargo test".to_string()),
            status: ProofCheckStatus::Passed,
            summary: None,
            evidence_ref: None,
            checked_at: now,
        });
        bundle.commits.push("e038ab5".to_string());
        bundle.blockers.push(ProofBlocker {
            blocker_id: Some("b1".to_string()),
            summary: "waiting for fixture".to_string(),
            source: None,
            status: ProofBlockerStatus::Open,
            created_at: now,
            resolved_at: None,
            resolution: None,
        });
        ProofSummary::from_bundle(&bundle)
    }

    #[test]
    fn proof_summary_is_included_when_present() {
        let summary = proof_summary_recorded();
        let mut input = base_input();
        input.proof_summary = Some(&summary);
        let prompt = build_review_request_prompt(&input);
        assert!(prompt.contains("Recorded proof of work so far:"));
        assert!(prompt.contains("Proof bundle proof-T-deadbeef"));
        assert!(prompt.contains("e038ab5"));
        assert!(prompt.contains("open blockers"));
    }

    #[test]
    fn proof_summary_block_highlights_missing_evidence() {
        let absent = ProofSummary::absent();
        let mut input = base_input();
        input.proof_summary = Some(&absent);
        let prompt = build_review_request_prompt(&input);
        assert!(prompt.contains("Recorded proof of work so far:"));
        assert!(prompt.contains("none recorded"));
    }

    #[test]
    fn proof_summary_block_does_not_replace_review_gate() {
        let summary = proof_summary_recorded();
        let mut input = base_input();
        input.proof_summary = Some(&summary);
        let prompt = build_review_request_prompt(&input);
        // Reviewers must still see the gate language requiring verification.
        assert!(prompt.contains("not approval"));
        assert!(prompt.contains("Verify every claim against the commit diff"));
        // The reviewer's verdict submission contract is unchanged.
        assert!(prompt.contains("verification action=submit_review"));
        assert!(prompt.contains("verdict=<approved|needs_revision|rejected>"));
    }

    #[test]
    fn proof_summary_block_is_omitted_when_none() {
        let prompt = build_review_request_prompt(&base_input());
        assert!(!prompt.contains("Recorded proof of work so far"));
    }
}

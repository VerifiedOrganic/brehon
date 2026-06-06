use super::*;
use brehon_types::ProofSummary;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn run_git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {}: {err}", args.join(" ")));
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn init_project() -> (TempDir, String) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    run_git(root, &["init", "-b", "main"]);
    run_git(root, &["config", "user.email", "test@example.com"]);
    run_git(root, &["config", "user.name", "Test User"]);
    fs::write(root.join("README.md"), "seed\n").unwrap();
    run_git(root, &["add", "README.md"]);
    run_git(root, &["commit", "-m", "seed"]);
    fs::create_dir_all(root.join(".brehon/runtime/reviews")).unwrap();
    fs::create_dir_all(root.join(".brehon/runtime/tasks")).unwrap();
    fs::create_dir_all(root.join(".brehon/runtime/proof")).unwrap();
    let head = run_git(root, &["rev-parse", "HEAD"]);
    (temp, head)
}

fn write_task(root: &Path, task_id: &str, commit: &str) {
    fs::write(
        root.join(".brehon/runtime/tasks")
            .join(format!("{task_id}.json")),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": task_id,
            "title": format!("Task {task_id}"),
            "status": "closed",
            "completion_mode": "merge",
            "integration_status": "integrated",
            "latest_commit": commit,
            "merged_commit": commit
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_proof(root: &Path, task_id: &str) {
    let mut proof = ProofSummary::absent();
    proof.absent = false;
    proof.status = "complete".to_string();
    proof.proof_bundle_id = Some(format!("proof-{task_id}"));
    proof.command_count = 1;
    proof.test_count = 1;
    proof.missing.clear();
    fs::write(
        root.join(".brehon/runtime/proof")
            .join(format!("{task_id}.json")),
        serde_json::to_string_pretty(&proof).unwrap(),
    )
    .unwrap();
}

fn write_review(
    root: &Path,
    task_id: &str,
    commit: &str,
    requested_reviewers: &[&str],
    scores: &[(&str, &str, bool)],
) {
    let round = root
        .join(".brehon/runtime/reviews")
        .join(task_id)
        .join("round-1");
    fs::create_dir_all(&round).unwrap();
    let prompts: BTreeMap<String, String> = requested_reviewers
        .iter()
        .map(|reviewer| ((*reviewer).to_string(), "review".to_string()))
        .collect();
    fs::write(
        round.join("request.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": task_id,
            "review_id": format!("REV-{task_id}"),
            "requested_by": "supervisor",
            "requested_at": "2026-06-06T00:00:00Z",
            "title": task_id,
            "description": "",
            "commit": commit,
            "commits": [commit],
            "reviewer_prompts": prompts,
            "context": ""
        }))
        .unwrap(),
    )
    .unwrap();

    let score_map: serde_json::Map<String, Value> = scores
        .iter()
        .map(|(reviewer, verdict, ignored)| {
            (
                (*reviewer).to_string(),
                serde_json::json!({
                    "score": 8,
                    "verdict": verdict,
                    "ignored_for_threshold": ignored
                }),
            )
        })
        .collect();
    let approvals = scores
        .iter()
        .filter(|(_, verdict, ignored)| *verdict == "approved" && !*ignored)
        .count();
    fs::write(
        round.join("consolidated.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "review_id": format!("REV-{task_id}"),
            "task_id": task_id,
            "round": 1,
            "outcome": "approved",
            "scores": score_map,
            "average_score": 8.0,
            "min_score": 8,
            "approval_count": approvals,
            "threshold_result": "Approved",
            "threshold_reason": "All thresholds met",
            "blocking": [],
            "suggestions": [],
            "nitpicks": [],
            "dissent": [],
            "evaluated_at": "2026-06-06T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn audit_trusts_full_panel_approved_commit_on_target() {
    let (temp, head) = init_project();
    let root = temp.path();
    write_task(root, "T-good", &head);
    write_proof(root, "T-good");
    write_review(
        root,
        "T-good",
        &head,
        &["r1", "r2", "r3"],
        &[
            ("r1", "approved", false),
            ("r2", "approved", false),
            ("r3", "approved", false),
        ],
    );

    let report = build_report(Some(root), "main", 100).unwrap();
    assert_eq!(report.counts.trusted, 1, "{:#?}", report.tasks);
    assert_eq!(report.tasks[0].bucket, AuditBucket::Trusted);
}

#[test]
fn audit_flags_two_of_three_approved() {
    let (temp, head) = init_project();
    let root = temp.path();
    write_task(root, "T-gap", &head);
    write_proof(root, "T-gap");
    write_review(
        root,
        "T-gap",
        &head,
        &["r1", "r2", "r3"],
        &[("r1", "approved", false), ("r2", "approved", false)],
    );

    let report = build_report(Some(root), "main", 100).unwrap();
    assert_eq!(report.counts.needs_rereview, 1);
    assert_eq!(report.tasks[0].bucket, AuditBucket::NeedsRereview);
    assert!(report.tasks[0]
        .reasons
        .iter()
        .any(|reason| reason.contains("missing_requested")));
}

#[test]
fn audit_manual_inspects_reassignment_ambiguity() {
    let (temp, head) = init_project();
    let root = temp.path();
    write_task(root, "T-reseat", &head);
    write_proof(root, "T-reseat");
    write_review(
        root,
        "T-reseat",
        &head,
        &["r1", "r2", "r3", "stale-r4"],
        &[
            ("r1", "approved", false),
            ("r2", "approved", false),
            ("r3", "approved", false),
        ],
    );

    let report = build_report(Some(root), "main", 100).unwrap();
    assert_eq!(report.counts.manual_inspect, 1, "{:#?}", report.tasks);
    assert_eq!(report.tasks[0].bucket, AuditBucket::ManualInspect);
    assert!(report.tasks[0]
        .reasons
        .iter()
        .any(|reason| reason.contains("reassignment_possible")));
}

#[test]
fn audit_flags_reviewed_commit_missing_from_target() {
    let (temp, _head) = init_project();
    let root = temp.path();
    run_git(root, &["checkout", "-b", "work"]);
    fs::write(root.join("feature.txt"), "work\n").unwrap();
    run_git(root, &["add", "feature.txt"]);
    run_git(root, &["commit", "-m", "work"]);
    let work_commit = run_git(root, &["rev-parse", "HEAD"]);
    run_git(root, &["checkout", "main"]);

    write_task(root, "T-missing", &work_commit);
    write_proof(root, "T-missing");
    write_review(
        root,
        "T-missing",
        &work_commit,
        &["r1", "r2", "r3"],
        &[
            ("r1", "approved", false),
            ("r2", "approved", false),
            ("r3", "approved", false),
        ],
    );

    let report = build_report(Some(root), "main", 100).unwrap();
    assert_eq!(report.counts.needs_rereview, 1);
    assert!(report.tasks[0]
        .reasons
        .iter()
        .any(|reason| reason.contains("reviewed_commit_missing_on_target")));
}

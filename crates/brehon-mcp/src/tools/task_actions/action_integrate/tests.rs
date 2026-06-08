use super::*;
use std::process::{Command, Output};

fn run_git(cwd: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {}: {err}", args.join(" ")))
}

fn run_git_ok(cwd: &Path, args: &[&str]) -> String {
    let output = run_git(cwd, args);
    assert!(
        output.status.success(),
        "git {} failed: {}{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn cherry_picking_probe_short_circuits_unmerged_conflict() {
    let repo = tempfile::tempdir().expect("tempdir");
    let cwd = repo.path();
    run_git_ok(cwd, &["init", "-b", "main"]);
    run_git_ok(cwd, &["config", "user.email", "brehon@example.invalid"]);
    run_git_ok(cwd, &["config", "user.name", "Brehon Test"]);

    std::fs::write(cwd.join("conflict.txt"), "base\n").unwrap();
    run_git_ok(cwd, &["add", "conflict.txt"]);
    run_git_ok(cwd, &["commit", "-m", "base"]);
    run_git_ok(cwd, &["checkout", "-b", "reviewed"]);
    std::fs::write(cwd.join("conflict.txt"), "reviewed\n").unwrap();
    run_git_ok(cwd, &["commit", "-am", "reviewed change"]);
    let reviewed_sha = run_git_ok(cwd, &["rev-parse", "HEAD"]);

    run_git_ok(cwd, &["checkout", "main"]);
    std::fs::write(cwd.join("conflict.txt"), "main\n").unwrap();
    run_git_ok(cwd, &["commit", "-am", "main change"]);
    let cherry_pick = run_git(cwd, &["cherry-pick", &reviewed_sha]);
    assert!(
        !cherry_pick.status.success(),
        "cherry-pick should create a conflict"
    );

    let state = IntegrationState {
        phase: IntegrationPhase::CherryPicking,
        ..IntegrationState::default()
    };
    let probes = run_git_probes(cwd, &state, "missing-branch").expect("cheap cherry-pick probes");

    assert!(probes.cherry_pick_in_progress);
    assert_eq!(
        probes.cherry_pick_sha.as_deref(),
        Some(reviewed_sha.as_str())
    );
    assert_eq!(probes.unmerged_files, vec!["conflict.txt"]);
    assert!(!probes.is_ancestor);
    assert!(!probes.is_patch_equivalent);
}

#[test]
fn cleared_cherry_picking_probe_accepts_bounded_tree_match_fallback() {
    let repo = tempfile::tempdir().expect("tempdir");
    let cwd = repo.path();
    run_git_ok(cwd, &["init", "-b", "main"]);
    run_git_ok(cwd, &["config", "user.email", "brehon@example.invalid"]);
    run_git_ok(cwd, &["config", "user.name", "Brehon Test"]);

    std::fs::write(cwd.join("needle.txt"), "base\n").unwrap();
    run_git_ok(cwd, &["add", "needle.txt"]);
    run_git_ok(cwd, &["commit", "-m", "base"]);
    let base_sha = run_git_ok(cwd, &["rev-parse", "HEAD"]);

    run_git_ok(cwd, &["checkout", "-b", "reviewed"]);
    std::fs::write(cwd.join("needle.txt"), "target\n").unwrap();
    run_git_ok(cwd, &["commit", "-am", "reviewed change"]);
    let reviewed_sha = run_git_ok(cwd, &["rev-parse", "HEAD"]);

    run_git_ok(cwd, &["checkout", "main"]);
    std::fs::write(cwd.join("needle.txt"), "intermediate\n").unwrap();
    run_git_ok(cwd, &["commit", "-am", "main intermediate"]);
    std::fs::write(cwd.join("needle.txt"), "target\n").unwrap();
    run_git_ok(cwd, &["commit", "-am", "main same final content"]);

    let state = IntegrationState {
        phase: IntegrationPhase::CherryPicking,
        reviewed_commits: vec![reviewed_sha.clone()],
        cherry_pick_base_head: base_sha,
        ..IntegrationState::default()
    };
    let probes = run_git_probes(cwd, &state, "main").expect("stale cherry-pick probes");

    assert!(!probes.cherry_pick_in_progress);
    assert!(probes.cherry_pick_branch_advanced);
    assert!(!probes.is_ancestor);
    assert!(!probes.is_patch_equivalent);
    assert!(!probes.has_reviewed_cherry_pick_trailers);
    assert!(probes.reviewed_commits_applied);
    assert!(
        probes.tree_matches_after,
        "clean cleared cherry-picking state should use bounded tree matching"
    );

    let resolved_state = IntegrationState {
        phase: IntegrationPhase::Resolved,
        reviewed_commits: vec![reviewed_sha],
        ..IntegrationState::default()
    };
    let resolved_probes = run_git_probes(cwd, &resolved_state, "main").expect("resolved probes");
    assert!(
        resolved_probes.tree_matches_after,
        "resolved verification still uses the tree fallback"
    );
}

#[test]
fn cleared_cherry_picking_probe_caps_tree_match_fallback() {
    let repo = tempfile::tempdir().expect("tempdir");
    let cwd = repo.path();
    run_git_ok(cwd, &["init", "-b", "main"]);
    run_git_ok(cwd, &["config", "user.email", "brehon@example.invalid"]);
    run_git_ok(cwd, &["config", "user.name", "Brehon Test"]);

    std::fs::write(cwd.join("README.md"), "base\n").unwrap();
    run_git_ok(cwd, &["add", "README.md"]);
    run_git_ok(cwd, &["commit", "-m", "base"]);

    run_git_ok(cwd, &["checkout", "-b", "reviewed"]);
    for idx in 0..=CLEARED_CHERRY_PICK_TREE_MATCH_FILE_LIMIT {
        std::fs::write(cwd.join(format!("file-{idx}.txt")), "target\n").unwrap();
    }
    run_git_ok(cwd, &["add", "."]);
    run_git_ok(cwd, &["commit", "-m", "reviewed large change"]);
    let reviewed_sha = run_git_ok(cwd, &["rev-parse", "HEAD"]);

    run_git_ok(cwd, &["checkout", "main"]);
    for idx in 0..=CLEARED_CHERRY_PICK_TREE_MATCH_FILE_LIMIT {
        std::fs::write(cwd.join(format!("file-{idx}.txt")), "intermediate\n").unwrap();
    }
    run_git_ok(cwd, &["add", "."]);
    run_git_ok(cwd, &["commit", "-m", "main intermediate files"]);
    for idx in 0..=CLEARED_CHERRY_PICK_TREE_MATCH_FILE_LIMIT {
        std::fs::write(cwd.join(format!("file-{idx}.txt")), "target\n").unwrap();
    }
    run_git_ok(cwd, &["commit", "-am", "main same large final content"]);

    let state = IntegrationState {
        phase: IntegrationPhase::CherryPicking,
        reviewed_commits: vec![reviewed_sha],
        ..IntegrationState::default()
    };
    let probes = run_git_probes(cwd, &state, "main").expect("capped cherry-pick probes");

    assert!(!probes.cherry_pick_in_progress);
    assert!(!probes.is_ancestor);
    assert!(!probes.is_patch_equivalent);
    assert!(!probes.has_reviewed_cherry_pick_trailers);
    assert!(
        !probes.reviewed_commits_applied,
        "large fallback must not count as applied without cheap proof"
    );
    assert!(
        !probes.tree_matches_after,
        "large fallback must stay capped to avoid expensive retry probes"
    );
}

#[test]
fn execute_cherry_picks_skips_represented_commit_and_continues_remaining() {
    let repo = tempfile::tempdir().expect("tempdir");
    let cwd = repo.path();
    run_git_ok(cwd, &["init", "-b", "main"]);
    run_git_ok(cwd, &["config", "user.email", "brehon@example.invalid"]);
    run_git_ok(cwd, &["config", "user.name", "Brehon Test"]);

    std::fs::write(cwd.join("needle.txt"), "base\n").unwrap();
    run_git_ok(cwd, &["add", "needle.txt"]);
    run_git_ok(cwd, &["commit", "-m", "base"]);

    run_git_ok(cwd, &["checkout", "-b", "reviewed"]);
    std::fs::write(cwd.join("needle.txt"), "target\n").unwrap();
    run_git_ok(cwd, &["commit", "-am", "reviewed first"]);
    let first_sha = run_git_ok(cwd, &["rev-parse", "HEAD"]);
    std::fs::write(cwd.join("second.txt"), "second\n").unwrap();
    run_git_ok(cwd, &["add", "second.txt"]);
    run_git_ok(cwd, &["commit", "-m", "reviewed second"]);
    let second_sha = run_git_ok(cwd, &["rev-parse", "HEAD"]);

    run_git_ok(cwd, &["checkout", "main"]);
    std::fs::write(cwd.join("needle.txt"), "target\nextra\n").unwrap();
    run_git_ok(cwd, &["commit", "-am", "resolved first with extra context"]);

    execute_cherry_picks(cwd, &[first_sha.clone(), second_sha.clone()], "")
        .expect("should skip represented first commit and continue second");

    assert_eq!(
        run_git_ok(cwd, &["show", "HEAD:needle.txt"]),
        "target\nextra"
    );
    assert_eq!(run_git_ok(cwd, &["show", "HEAD:second.txt"]), "second");
    assert!(
        !git_commit_is_ancestor_in(cwd, &first_sha, "HEAD").unwrap(),
        "first commit should be represented without re-picking its SHA"
    );
    let latest_message = run_git_ok(cwd, &["log", "-1", "--format=%B"]);
    assert!(
        latest_message.contains(&second_sha),
        "second commit should be cherry-picked with provenance trailer"
    );
}

#[test]
fn execute_cherry_picks_skips_commit_with_trailer_and_continues_remaining() {
    let repo = tempfile::tempdir().expect("tempdir");
    let cwd = repo.path();
    run_git_ok(cwd, &["init", "-b", "main"]);
    run_git_ok(cwd, &["config", "user.email", "brehon@example.invalid"]);
    run_git_ok(cwd, &["config", "user.name", "Brehon Test"]);

    std::fs::write(cwd.join("needle.txt"), "base\n").unwrap();
    run_git_ok(cwd, &["add", "needle.txt"]);
    run_git_ok(cwd, &["commit", "-m", "base"]);
    let base_head = run_git_ok(cwd, &["rev-parse", "HEAD"]);

    run_git_ok(cwd, &["checkout", "-b", "reviewed"]);
    std::fs::write(cwd.join("needle.txt"), "target\n").unwrap();
    run_git_ok(cwd, &["commit", "-am", "reviewed first"]);
    let first_sha = run_git_ok(cwd, &["rev-parse", "HEAD"]);
    std::fs::write(cwd.join("second.txt"), "second\n").unwrap();
    run_git_ok(cwd, &["add", "second.txt"]);
    run_git_ok(cwd, &["commit", "-m", "reviewed second"]);
    let second_sha = run_git_ok(cwd, &["rev-parse", "HEAD"]);

    run_git_ok(cwd, &["checkout", "main"]);
    std::fs::write(cwd.join("needle.txt"), "manual resolution\n").unwrap();
    let trailer = format!("(cherry picked from commit {first_sha})");
    run_git_ok(
        cwd,
        &[
            "commit",
            "-am",
            "manual first resolution with trailer",
            "-m",
            trailer.as_str(),
        ],
    );

    execute_cherry_picks(cwd, &[first_sha.clone(), second_sha.clone()], &base_head)
        .expect("should skip trailer-proven first commit and continue second");

    assert_eq!(
        run_git_ok(cwd, &["show", "HEAD:needle.txt"]),
        "manual resolution"
    );
    assert_eq!(run_git_ok(cwd, &["show", "HEAD:second.txt"]), "second");
    assert!(
        !git_commit_is_ancestor_in(cwd, &first_sha, "HEAD").unwrap(),
        "first commit should be represented by trailer without re-picking its SHA"
    );
    let latest_message = run_git_ok(cwd, &["log", "-1", "--format=%B"]);
    assert!(
        latest_message.contains(&second_sha),
        "second commit should still be cherry-picked after skipping trailer-proven first"
    );
}

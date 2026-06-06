use crate::tools::verification::{reviewed_commits, ReviewRequestFile};
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

pub(super) fn integration_commit_metadata(
    task_data: &serde_json::Map<String, Value>,
    review_request: Option<&ReviewRequestFile>,
    allow_latest_commit_fallback: bool,
) -> (String, Vec<String>, bool, bool) {
    if let Some(request) = review_request {
        let reviewed_commit = request.commit.trim().to_string();
        return (
            reviewed_commit,
            reviewed_commits(request),
            request.resolved_empty_commit_set,
            true,
        );
    }
    if !allow_latest_commit_fallback {
        return (String::new(), Vec::new(), false, false);
    }

    let latest_commit = task_data
        .get("latest_commit")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_default();
    let reviewed_commit_set = if latest_commit.is_empty() {
        Vec::new()
    } else {
        vec![latest_commit.clone()]
    };
    (latest_commit, reviewed_commit_set, false, false)
}

pub(super) fn reviewed_commits_with_cherry_pick_trailers(
    worktree: &Path,
    branch: &str,
    base_head: &str,
    reviewed_commits: &[String],
) -> HashSet<String> {
    if reviewed_commits.is_empty() {
        return HashSet::new();
    }
    if !base_head.is_empty() {
        let matches =
            reviewed_commits_with_cherry_pick_trailers_since(worktree, base_head, reviewed_commits)
                .unwrap_or_default();
        if matches.len() == reviewed_commits.len() {
            return matches;
        }
    }
    reviewed_commits_with_cherry_pick_trailers_in_branch_window(worktree, branch, reviewed_commits)
        .unwrap_or_default()
}

fn reviewed_commits_with_cherry_pick_trailers_since(
    worktree: &Path,
    base_head: &str,
    reviewed_commits: &[String],
) -> Result<HashSet<String>, String> {
    let range = format!("{base_head}..HEAD");
    let output = crate::git_exec::run_git(worktree, &["log", "--format=%H%x1e%B%x1f", &range])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!(
                "git log --format=%B%x1e {range} exited with status {}",
                output.status
            )
        } else {
            stderr
        });
    }
    reviewed_commits_with_trailers_from_log(&output.stdout, reviewed_commits)
}

fn reviewed_commits_with_cherry_pick_trailers_in_branch_window(
    worktree: &Path,
    branch: &str,
    reviewed_commits: &[String],
) -> Result<HashSet<String>, String> {
    let output = crate::git_exec::run_git(
        worktree,
        &["log", "--max-count=200", "--format=%H%x1e%B%x1f", branch],
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!(
                "git log --max-count=200 --format=%B%x1e {branch} exited with status {}",
                output.status
            )
        } else {
            stderr
        });
    }
    reviewed_commits_with_trailers_from_log(&output.stdout, reviewed_commits)
}

fn reviewed_commits_with_trailers_from_log(
    stdout: &[u8],
    reviewed_commits: &[String],
) -> Result<HashSet<String>, String> {
    let history = String::from_utf8_lossy(stdout);
    let commit_bodies: Vec<&str> = history
        .split('\u{1f}')
        .filter_map(|entry| entry.split_once('\u{1e}').map(|(_, body)| body))
        .collect();
    let mut matches = HashSet::new();
    for reviewed_commit in reviewed_commits {
        let needle = format!("(cherry picked from commit {reviewed_commit})");
        if commit_bodies.iter().any(|body| body.contains(&needle)) {
            matches.insert(reviewed_commit.clone());
        }
    }
    Ok(matches)
}

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};

use super::{ReviewedCommitAudit, ReviewedCommitStatus};

pub(super) struct GitInspector {
    root: PathBuf,
    target: String,
    max_target_commits: usize,
    repo_available: bool,
    target_available: bool,
    target_patch_ids: Option<HashSet<String>>,
}

impl GitInspector {
    pub(super) fn new(root: &Path, target: &str, max_target_commits: usize) -> Self {
        let repo_available = run_git_status(root, &["rev-parse", "--is-inside-work-tree"])
            .map(|status| status.success())
            .unwrap_or(false);
        let target_ref = format!("{target}^{{commit}}");
        let target_available = repo_available
            && run_git_status(root, &["rev-parse", "--verify", &target_ref])
                .map(|status| status.success())
                .unwrap_or(false);
        Self {
            root: root.to_path_buf(),
            target: target.to_string(),
            max_target_commits,
            repo_available,
            target_available,
            target_patch_ids: None,
        }
    }

    pub(super) fn commit_evidence(&mut self, commit: &str) -> ReviewedCommitAudit {
        let commit = commit.trim().to_string();
        if commit.is_empty() {
            return ReviewedCommitAudit {
                commit,
                status: ReviewedCommitStatus::Unknown,
                detail: "empty commit id".to_string(),
            };
        }
        if !self.repo_available {
            return ReviewedCommitAudit {
                commit,
                status: ReviewedCommitStatus::Unknown,
                detail: "git repository unavailable".to_string(),
            };
        }
        let commit_ref = format!("{commit}^{{commit}}");
        if !self.git_status(&["cat-file", "-e", &commit_ref]) {
            return ReviewedCommitAudit {
                commit,
                status: ReviewedCommitStatus::Unknown,
                detail: "commit object unavailable locally".to_string(),
            };
        }
        if !self.target_available {
            return ReviewedCommitAudit {
                commit,
                status: ReviewedCommitStatus::Unknown,
                detail: format!("target '{}' unavailable", self.target),
            };
        }
        if self.git_status(&["merge-base", "--is-ancestor", &commit, &self.target]) {
            return ReviewedCommitAudit {
                commit,
                status: ReviewedCommitStatus::Ancestor,
                detail: "commit is ancestor of target".to_string(),
            };
        }
        if self.target_log_mentions(&commit) {
            return ReviewedCommitAudit {
                commit,
                status: ReviewedCommitStatus::CherryPickTrailer,
                detail: "target log mentions reviewed commit".to_string(),
            };
        }
        if self.target_has_patch_id_for(&commit).unwrap_or(false) {
            return ReviewedCommitAudit {
                commit,
                status: ReviewedCommitStatus::PatchEquivalent,
                detail: "target contains patch-id equivalent commit".to_string(),
            };
        }

        ReviewedCommitAudit {
            commit,
            status: ReviewedCommitStatus::Missing,
            detail: format!("commit is not present on target '{}'", self.target),
        }
    }

    pub(super) fn commits_patch_equivalent(&mut self, left: &str, right: &str) -> Result<bool> {
        let Some(left_id) = self.patch_id_for_commit(left)? else {
            return Ok(false);
        };
        let Some(right_id) = self.patch_id_for_commit(right)? else {
            return Ok(false);
        };
        Ok(left_id == right_id)
    }

    fn target_has_patch_id_for(&mut self, commit: &str) -> Result<bool> {
        let Some(patch_id) = self.patch_id_for_commit(commit)? else {
            return Ok(false);
        };
        let target_ids = self.target_patch_ids()?;
        Ok(target_ids.contains(&patch_id))
    }

    fn target_patch_ids(&mut self) -> Result<&HashSet<String>> {
        if self.target_patch_ids.is_none() {
            let commits = self.git_stdout(&[
                "log",
                "--format=%H",
                "--no-ext-diff",
                "-n",
                &self.max_target_commits.to_string(),
                &self.target,
            ])?;
            let mut patch_ids = HashSet::new();
            for commit in commits
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
            {
                if let Some(patch_id) = self.patch_id_for_commit(commit)? {
                    patch_ids.insert(patch_id);
                }
            }
            self.target_patch_ids = Some(patch_ids);
        }
        Ok(self
            .target_patch_ids
            .as_ref()
            .expect("patch ids initialized"))
    }

    fn patch_id_for_commit(&self, commit: &str) -> Result<Option<String>> {
        let show = Command::new("git")
            .args([
                "show",
                "--pretty=format:",
                "--patch",
                "--no-ext-diff",
                commit,
            ])
            .current_dir(&self.root)
            .output()
            .with_context(|| format!("failed to run git show for {commit}"))?;
        if !show.status.success() || show.stdout.is_empty() {
            return Ok(None);
        }

        let mut child = Command::new("git")
            .args(["patch-id", "--stable"])
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("failed to run git patch-id")?;
        {
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| anyhow!("git patch-id stdin unavailable"))?;
            stdin
                .write_all(&show.stdout)
                .context("failed to write patch to git patch-id")?;
        }
        let output = child
            .wait_with_output()
            .context("failed to read git patch-id output")?;
        if !output.status.success() {
            return Ok(None);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout
            .split_whitespace()
            .next()
            .map(str::to_string)
            .filter(|value| !value.is_empty()))
    }

    fn target_log_mentions(&self, commit: &str) -> bool {
        self.git_stdout(&[
            "log",
            "--format=%B",
            "--fixed-strings",
            "--grep",
            commit,
            &self.target,
        ])
        .map(|stdout| stdout.contains(commit))
        .unwrap_or(false)
    }

    fn git_status(&self, args: &[&str]) -> bool {
        run_git_status(&self.root, args)
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn git_stdout(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .with_context(|| format!("failed to run git {}", args.join(" ")))?;
        if !output.status.success() {
            return Err(anyhow!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

fn run_git_status(root: &Path, args: &[&str]) -> Result<std::process::ExitStatus> {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .with_context(|| format!("failed to run git {}", args.join(" ")))
}

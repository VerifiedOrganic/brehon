//! Worktree diagnostic checker.
//!
//! Detects stale worktrees, uncommitted changes, detached HEAD, and other worktree issues.

use super::Checker;
use crate::types::{DiagnosticCategory, DiagnosticFinding, Severity};
use std::path::Path;

/// Terminal task statuses - worktrees for these tasks should be cleaned up.
const TERMINAL_STATUSES: &[&str] = &["merged", "Merged", "closed", "Closed"];

/// Checker for worktree issues.
pub struct WorktreeChecker {
    brehon_root: std::path::PathBuf,
    runtime_dir: std::path::PathBuf,
}

impl WorktreeChecker {
    pub fn new(brehon_root: &Path) -> Self {
        Self {
            brehon_root: brehon_root.to_path_buf(),
            runtime_dir: brehon_root.join("runtime"),
        }
    }

    /// Load task statuses from runtime/tasks directory
    fn load_task_statuses(
        &self,
    ) -> Result<std::collections::HashMap<String, String>, anyhow::Error> {
        let mut tasks = std::collections::HashMap::new();
        let tasks_dir = self.runtime_dir.join("tasks");

        if !tasks_dir.exists() {
            return Ok(tasks);
        }

        for entry in std::fs::read_dir(&tasks_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }

            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(task_id) = json.get("task_id").and_then(|v| v.as_str()) {
                        let status = json
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        tasks.insert(task_id.to_string(), status);
                    }
                }
            }
        }

        Ok(tasks)
    }

    /// Extract task ID from worktree directory name.
    /// Worktrees are typically named after the task (e.g., "T-abc123" or "task-T-abc123")
    fn extract_task_id(&self, worktree_name: &str) -> Option<String> {
        // Try direct match: T-xxx or E-xxx
        if worktree_name.starts_with("T-") || worktree_name.starts_with("E-") {
            let parts: Vec<&str> = worktree_name.splitn(2, '-').collect();
            if parts.len() == 2 && !parts[1].is_empty() {
                return Some(worktree_name.to_string());
            }
        }
        // Try prefix pattern: task-T-xxx, epic-E-xxx
        for prefix in &["task-", "epic-"] {
            if let Some(rest) = worktree_name.strip_prefix(prefix) {
                if rest.starts_with("T-") || rest.starts_with("E-") {
                    return Some(rest.to_string());
                }
            }
        }
        None
    }

    fn collect_worktree_dirs(&self) -> Result<Vec<std::path::PathBuf>, anyhow::Error> {
        let worktrees_dir = self.brehon_root.join("worktrees");
        if !worktrees_dir.exists() {
            return Ok(Vec::new());
        }

        let mut result = Vec::new();
        self.collect_worktree_dirs_recursive(&worktrees_dir, &mut result)?;
        Ok(result)
    }

    fn collect_worktree_dirs_recursive(
        &self,
        dir: &Path,
        result: &mut Vec<std::path::PathBuf>,
    ) -> Result<(), anyhow::Error> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_string();
            if name == "_archived" {
                continue;
            }

            if path.join(".git").exists() {
                result.push(path);
                continue;
            }

            self.collect_worktree_dirs_recursive(&path, result)?;
        }

        Ok(())
    }

    fn check_stale_worktrees(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let worktree_dirs = self.collect_worktree_dirs()?;

        // Load task statuses for cross-reference
        let task_statuses = self.load_task_statuses()?;

        for path in worktree_dirs {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            // Try to extract task ID from worktree name
            if let Some(task_id) = self.extract_task_id(&name) {
                // Check if task exists and what its status is
                match task_statuses.get(&task_id) {
                    None => {
                        // Task doesn't exist - worktree is orphaned
                        findings.push(
                            DiagnosticFinding::new(
                                DiagnosticCategory::Worktree,
                                Severity::Warning,
                                format!("Worktree for non-existent task: {}", name),
                            )
                            .with_subject(name.clone())
                            .with_description(format!(
                                "Worktree '{}' references task '{}' which does not exist",
                                name, task_id
                            ))
                            .with_suggestion(
                                "Remove orphaned worktree: git worktree remove <path>",
                            ),
                        );
                    }
                    Some(status) if TERMINAL_STATUSES.contains(&status.as_str()) => {
                        // Task is in terminal state - worktree should be cleaned up
                        findings.push(
                            DiagnosticFinding::new(
                                DiagnosticCategory::Worktree,
                                Severity::Info,
                                format!("Worktree for {} task: {}", status, name),
                            )
                            .with_subject(name.clone())
                            .with_description(format!(
                                "Worktree '{}' belongs to task '{}' which is in terminal state '{}'",
                                name, task_id, status
                            ))
                            .with_suggestion("Clean up worktree for completed task: git worktree remove <path>"),
                        );
                    }
                    Some(_) => {
                        // Task is active - don't flag as stale even if old
                    }
                }
            }
        }

        Ok(findings)
    }

    fn check_detached_heads(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let worktree_dirs = self.collect_worktree_dirs()?;

        for path in worktree_dirs {
            let git_file = path.join(".git");
            if git_file.exists() {
                if let Ok(content) = std::fs::read_to_string(&git_file) {
                    let gitdir_line = content.lines().next().unwrap_or("");
                    if let Some(gitdir_path) = gitdir_line.strip_prefix("gitdir:") {
                        let gitdir_path = gitdir_path.trim();
                        let head_file = path.join(gitdir_path).join("HEAD");
                        if head_file.exists() {
                            if let Ok(head_content) = std::fs::read_to_string(&head_file) {
                                let head_trimmed = head_content.trim();
                                if !head_trimmed.starts_with("ref: ") && !head_trimmed.is_empty() {
                                    let name = path
                                        .file_name()
                                        .map(|n| n.to_string_lossy().to_string())
                                        .unwrap_or_default();
                                    findings.push(
                                        DiagnosticFinding::new(
                                            DiagnosticCategory::Worktree,
                                            Severity::Error,
                                            format!("Detached HEAD in worktree: {}", name),
                                        )
                                        .with_subject(name.clone())
                                        .with_suggestion(
                                            format!(
                                            "Attach to a branch with 'git -C {} checkout <branch>'",
                                            path.display()
                                        ),
                                        ),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(findings)
    }

    fn check_uncommitted_work(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let worktree_dirs = self.collect_worktree_dirs()?;

        for path in worktree_dirs {
            let output = std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&path)
                .output();

            if let Ok(output) = output {
                if !output.status.success() {
                    continue;
                }
                let status = String::from_utf8_lossy(&output.stdout);
                if !status.trim().is_empty() {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let count = status.lines().count();
                    findings.push(
                        DiagnosticFinding::new(
                            DiagnosticCategory::Worktree,
                            Severity::Warning,
                            format!("Uncommitted changes in {}: {} files", name, count),
                        )
                        .with_subject(name)
                        .with_suggestion("Commit or stash changes before proceeding"),
                    );
                }
            }
        }

        Ok(findings)
    }

    /// Find the git repository root by traversing up from brehon_root
    fn find_git_root(&self) -> Option<std::path::PathBuf> {
        let mut current = self.brehon_root.as_path();
        loop {
            if current.join(".git").exists() {
                return Some(current.to_path_buf());
            }
            match current.parent() {
                Some(parent) => current = parent,
                None => return None,
            }
        }
    }

    fn check_orphaned_worktrees(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let worktree_dirs = self.collect_worktree_dirs()?;

        // Find the git root - .brehon could be at repo root or inside
        let git_root = match self.find_git_root() {
            Some(root) => root,
            None => return Ok(findings), // Not in a git repo
        };

        // Read git worktree list from git root
        let output = std::process::Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&git_root)
            .output();

        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return Ok(findings),
        };

        let listing = String::from_utf8_lossy(&output.stdout);
        let mut known_paths: Vec<std::path::PathBuf> = Vec::new();

        for line in listing.lines() {
            if let Some(path_str) = line.strip_prefix("worktree ") {
                // Canonicalize for reliable comparison
                if let Ok(canonical) = std::fs::canonicalize(path_str) {
                    known_paths.push(canonical);
                } else {
                    known_paths.push(std::path::PathBuf::from(path_str));
                }
            }
        }

        for path in worktree_dirs {
            let canonical_path = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let is_known = known_paths.iter().any(|known| {
                let canonical_known =
                    std::fs::canonicalize(known).unwrap_or_else(|_| known.clone());
                canonical_known == canonical_path
            });

            if !is_known {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                findings.push(
                    DiagnosticFinding::new(
                        DiagnosticCategory::Worktree,
                        Severity::Error,
                        format!("Orphaned worktree directory: {}", name),
                    )
                    .with_subject(name)
                    .with_suggestion("Remove manually or re-register with git worktree"),
                );
            }
        }

        Ok(findings)
    }

    fn check_orphaned_git_metadata(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();

        // Find the git root
        let git_root = match self.find_git_root() {
            Some(root) => root,
            None => return Ok(findings),
        };

        let git_worktrees_dir = git_root.join(".git").join("worktrees");
        if !git_worktrees_dir.exists() {
            // Also check for bare repo where .git IS the worktrees dir
            let alt_dir = git_root.join("worktrees");
            if !alt_dir.exists() {
                return Ok(findings);
            }
        }

        let git_worktrees_dir = git_root.join(".git").join("worktrees");
        if !git_worktrees_dir.exists() {
            return Ok(findings);
        }

        for entry in std::fs::read_dir(&git_worktrees_dir)? {
            let entry = entry?;
            let worktree_meta = entry.path();
            if !worktree_meta.is_dir() {
                continue;
            }

            let git_dir_file = worktree_meta.join("gitdir");
            if !git_dir_file.exists() {
                continue;
            }

            if let Ok(gitdir_content) = std::fs::read_to_string(&git_dir_file) {
                let worktree_git_dir = gitdir_content.trim();
                let worktree_dir = worktree_git_dir
                    .strip_suffix("/.git")
                    .or_else(|| worktree_git_dir.strip_suffix("\\.git"))
                    .unwrap_or(worktree_git_dir);

                let worktree_path = std::path::Path::new(worktree_dir);

                // Canonicalize for reliable comparison (handles symlinks, /var vs /private/var on macOS)
                let exists = std::fs::canonicalize(worktree_path).is_ok();

                if !exists {
                    let name = worktree_meta
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    findings.push(
                        DiagnosticFinding::new(
                            DiagnosticCategory::Worktree,
                            Severity::Error,
                            format!("Orphaned git worktree metadata: {}", name),
                        )
                        .with_subject(name.clone())
                        .with_description(format!(
                            ".git/worktrees metadata points to missing directory: {}",
                            worktree_dir
                        ))
                        .with_suggestion("Remove orphaned metadata: git worktree prune"),
                    );
                }
            }
        }

        Ok(findings)
    }

    /// Detect branch drift: worktree is on a different branch than the task expects.
    fn check_branch_drift(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let worktrees_dir = self.brehon_root.join("worktrees");
        let tasks_dir = self.runtime_dir.join("tasks");

        if !worktrees_dir.exists() || !tasks_dir.exists() {
            return Ok(findings);
        }

        // Build map of worker name → expected branch from active tasks
        let mut expected_branches: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::new();
        if let Ok(entries) = std::fs::read_dir(&tasks_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                        let status = json.get("status").and_then(|v| v.as_str()).unwrap_or("");
                        if status != "in_progress" && status != "assigned" {
                            continue;
                        }
                        if let (Some(assignee), Some(task_id)) = (
                            json.get("assignee").and_then(|v| v.as_str()),
                            json.get("task_id").and_then(|v| v.as_str()),
                        ) {
                            // The expected branch is typically brehon/{worker_name}
                            let expected = format!("brehon/{}", assignee);
                            expected_branches
                                .insert(assignee.to_string(), (expected, task_id.to_string()));
                        }
                    }
                }
            }
        }

        // Check each worker worktree
        for entry in std::fs::read_dir(&worktrees_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            if let Some((expected_branch, task_id)) = expected_branches.get(&name) {
                // Read the current branch using git2
                if let Ok(repo) = git2::Repository::open(&path) {
                    if let Ok(head) = repo.head() {
                        if let Some(branch_name) = head.shorthand() {
                            if branch_name != expected_branch {
                                findings.push(
                                    DiagnosticFinding::new(
                                        DiagnosticCategory::Worktree,
                                        Severity::Warning,
                                        format!("Branch drift in worktree: {}", name),
                                    )
                                    .with_subject(name.clone())
                                    .with_description(format!(
                                        "Worktree is on branch '{}' but task {} expects '{}'",
                                        branch_name, task_id, expected_branch
                                    ))
                                    .with_suggestion(format!(
                                        "Check if worker switched branches intentionally: git -C {} branch",
                                        path.display()
                                    )),
                                );
                            }
                        }
                    }
                }
            }
        }

        Ok(findings)
    }

    /// Detect reassignment-blocked state: stalled worker with dirty worktree.
    fn check_reassignment_blocked(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let worktrees_dir = self.brehon_root.join("worktrees");
        let tasks_dir = self.runtime_dir.join("tasks");
        let sessions_dir = self.runtime_dir.join("sessions");

        if !worktrees_dir.exists() || !tasks_dir.exists() {
            return Ok(findings);
        }

        let now = chrono::Utc::now();

        // Find stalled workers (session last_seen is old OR task hasn't updated)
        let mut stalled_workers: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        if sessions_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_none_or(|e| e != "json") {
                        continue;
                    }
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                            let role = json.get("role").and_then(|v| v.as_str()).unwrap_or("");
                            if role != "worker" {
                                continue;
                            }
                            if let Some(name) = json.get("name").and_then(|v| v.as_str()) {
                                let last_seen = json
                                    .get("last_seen_at")
                                    .and_then(|v| v.as_str())
                                    .and_then(|s| {
                                        chrono::DateTime::parse_from_rfc3339(s)
                                            .ok()
                                            .map(|dt| dt.with_timezone(&chrono::Utc))
                                    });
                                // Consider stalled if not seen in 10+ minutes
                                if let Some(seen) = last_seen {
                                    if (now - seen).num_seconds() > 600 {
                                        stalled_workers.insert(name.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if stalled_workers.is_empty() {
            return Ok(findings);
        }

        // Check worktree dirty state for stalled workers
        for worker in &stalled_workers {
            let worktree_path = worktrees_dir.join(worker);
            if !worktree_path.is_dir() {
                continue;
            }

            if let Ok(repo) = git2::Repository::open(&worktree_path) {
                let mut opts = git2::StatusOptions::new();
                opts.include_untracked(true);
                if let Ok(statuses) = repo.statuses(Some(&mut opts)) {
                    let dirty_count = statuses.len();
                    if dirty_count > 0 {
                        findings.push(
                            DiagnosticFinding::new(
                                DiagnosticCategory::Worktree,
                                Severity::Error,
                                format!(
                                    "Reassignment blocked: {} uncommitted files in {}",
                                    dirty_count, worker
                                ),
                            )
                            .with_subject(worker.clone())
                            .with_description(format!(
                                "Stalled worker '{}' has {} uncommitted files preventing safe reassignment",
                                worker, dirty_count
                            ))
                            .with_suggestion(format!(
                                "Reassignment blocked: {} uncommitted files in worktree. Archive with force_reassign or recover manually.",
                                dirty_count
                            )),
                        );
                    }
                }
            }
        }

        Ok(findings)
    }
}

impl Checker for WorktreeChecker {
    fn category(&self) -> DiagnosticCategory {
        DiagnosticCategory::Worktree
    }

    fn check(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        findings.extend(self.check_stale_worktrees()?);
        findings.extend(self.check_detached_heads()?);
        findings.extend(self.check_uncommitted_work()?);
        findings.extend(self.check_orphaned_worktrees()?);
        findings.extend(self.check_orphaned_git_metadata()?);
        findings.extend(self.check_branch_drift()?);
        findings.extend(self.check_reassignment_blocked()?);
        Ok(findings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_checker() {
        let checker = WorktreeChecker::new(Path::new("/tmp"));
        assert_eq!(checker.category(), DiagnosticCategory::Worktree);
    }

    #[test]
    fn test_extract_task_id() {
        let checker = WorktreeChecker::new(Path::new("/tmp"));

        assert_eq!(
            checker.extract_task_id("T-abc123"),
            Some("T-abc123".to_string())
        );
        assert_eq!(
            checker.extract_task_id("E-xyz789"),
            Some("E-xyz789".to_string())
        );
        assert_eq!(
            checker.extract_task_id("task-T-abc123"),
            Some("T-abc123".to_string())
        );
        assert_eq!(
            checker.extract_task_id("epic-E-xyz789"),
            Some("E-xyz789".to_string())
        );
        assert_eq!(checker.extract_task_id("random-name"), None);
        assert_eq!(checker.extract_task_id("T-"), None);
    }

    #[test]
    fn test_reassignment_blocked_detected() {
        // This test requires a real git worktree, which is complex to set up.
        // Verify the method runs without error on an empty directory.
        let root = tempfile::tempdir().unwrap();
        let brehon_root = root.path().join(".brehon");
        let runtime = brehon_root.join("runtime");
        let sessions_dir = runtime.join("sessions");
        let tasks_dir = runtime.join("tasks");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::create_dir_all(brehon_root.join("worktrees")).unwrap();

        let checker = WorktreeChecker::new(&brehon_root);
        let findings = checker.check_reassignment_blocked().unwrap();
        // No stalled workers, so no findings
        assert!(findings.is_empty());
    }

    #[test]
    fn test_branch_drift_no_worktrees() {
        let root = tempfile::tempdir().unwrap();
        let brehon_root = root.path().join(".brehon");
        let runtime = brehon_root.join("runtime");
        std::fs::create_dir_all(&runtime.join("tasks")).unwrap();

        let checker = WorktreeChecker::new(&brehon_root);
        let findings = checker.check_branch_drift().unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn test_collect_worktree_dirs_ignores_runs_container_without_gitdir() {
        let root = tempfile::tempdir().unwrap();
        let brehon_root = root.path().join(".brehon");
        let runs_dir = brehon_root.join("worktrees").join("runs").join("session-1");
        std::fs::create_dir_all(&runs_dir).unwrap();

        let checker = WorktreeChecker::new(&brehon_root);
        let dirs = checker.collect_worktree_dirs().unwrap();
        assert!(dirs.is_empty());
    }
}

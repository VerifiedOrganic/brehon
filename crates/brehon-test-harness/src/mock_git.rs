//! Fake GitOperations implementation for testing.
//!
//! Simulated repository state in memory with worktree management,
//! rebase/merge simulation, and conflict detection.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;

use brehon_ports::{
    ConflictEntry, ConflictType, Diff, FileDiff, GitOperations, MergeResult, PortError,
    RebaseFallbackStrategy, RebaseResult,
};

/// Simulated git repository state.
#[derive(Debug, Clone)]
struct RepoState {
    branches: HashMap<String, BranchState>,
    current_branch: String,
    worktrees: HashMap<String, String>,
    merge_state: Option<MergeState>,
    rebase_state: Option<RebaseState>,
}

#[derive(Debug, Clone)]
struct BranchState {
    commits: Vec<String>,
    files: HashMap<String, FileContent>,
}

#[derive(Debug, Clone)]
pub struct FileContent {
    pub content: String,
    pub lines_added: usize,
    pub lines_removed: usize,
}

#[derive(Debug, Clone)]
struct MergeState {
    source_branch: String,
    conflicting_files: Vec<String>,
}

#[derive(Debug, Clone)]
struct RebaseState {
    conflicting_files: Vec<String>,
}

/// Fake Git operations implementation for testing.
///
/// Simulates repository state in memory without requiring actual git.
#[derive(Debug, Clone)]
pub struct FakeGitOperations {
    inner: Arc<RwLock<RepoState>>,
}

impl FakeGitOperations {
    pub fn new() -> Self {
        let mut branches = HashMap::new();
        branches.insert(
            "main".to_string(),
            BranchState {
                commits: vec!["initial".to_string()],
                files: HashMap::new(),
            },
        );

        Self {
            inner: Arc::new(RwLock::new(RepoState {
                branches,
                current_branch: "main".to_string(),
                worktrees: HashMap::new(),
                merge_state: None,
                rebase_state: None,
            })),
        }
    }

    pub fn create_branch(&self, name: &str) {
        let inner = self.inner.read();
        let base = inner
            .branches
            .get(&inner.current_branch)
            .cloned()
            .expect("current branch must exist");

        let new_branch = BranchState {
            commits: base.commits.clone(),
            files: base.files.clone(),
        };

        drop(inner);
        self.inner
            .write()
            .branches
            .insert(name.to_string(), new_branch);
    }

    pub fn add_conflict_files(&self, branch: &str, files: Vec<String>) {
        let mut inner = self.inner.write();
        if let Some(merge) = &mut inner.merge_state {
            if merge.source_branch == branch {
                merge.conflicting_files = files;
            }
        }
    }

    pub fn set_rebase_conflict(&self, files: Vec<String>) {
        let mut inner = self.inner.write();
        inner.rebase_state = Some(RebaseState {
            conflicting_files: files,
        });
    }

    pub fn set_merge_conflict(&self, branch: &str, files: Vec<String>) {
        let mut inner = self.inner.write();
        inner.merge_state = Some(MergeState {
            source_branch: branch.to_string(),
            conflicting_files: files,
        });
    }

    pub fn set_branch_files(&self, branch: &str, files: HashMap<String, FileContent>) {
        let mut inner = self.inner.write();
        if let Some(b) = inner.branches.get_mut(branch) {
            b.files = files;
        }
    }

    pub fn current_branch_name(&self) -> String {
        self.inner.read().current_branch.clone()
    }

    pub fn branch_exists(&self, name: &str) -> bool {
        self.inner.read().branches.contains_key(name)
    }
}

impl Default for FakeGitOperations {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl GitOperations for FakeGitOperations {
    async fn create_worktree(&self, branch: &str, path: &Path) -> Result<(), PortError> {
        let mut inner = self.inner.write();

        if !inner.branches.contains_key(branch) {
            return Err(PortError::Git(format!(
                "branch '{}' does not exist",
                branch
            )));
        }

        let path_str = path.to_string_lossy().to_string();
        if inner.worktrees.contains_key(&path_str) {
            return Err(PortError::Git("worktree path already exists".into()));
        }

        inner.worktrees.insert(path_str, branch.to_string());
        Ok(())
    }

    async fn create_branch(&self, name: &str, base_ref: Option<&str>) -> Result<(), PortError> {
        let inner = self.inner.read();
        let base_name = base_ref.unwrap_or(&inner.current_branch);
        let base =
            inner.branches.get(base_name).cloned().ok_or_else(|| {
                PortError::Git(format!("base branch '{}' does not exist", base_name))
            })?;

        drop(inner);
        self.inner.write().branches.insert(name.to_string(), base);
        Ok(())
    }

    async fn delete_branch(&self, name: &str) -> Result<(), PortError> {
        let mut inner = self.inner.write();

        if inner.current_branch == name {
            return Err(PortError::Git("cannot delete the current branch".into()));
        }

        if inner.branches.remove(name).is_none() {
            return Err(PortError::Git(format!("branch '{}' does not exist", name)));
        }

        Ok(())
    }

    async fn remove_worktree(&self, path: &Path) -> Result<(), PortError> {
        let mut inner = self.inner.write();
        let path_str = path.to_string_lossy().to_string();

        if inner.worktrees.remove(&path_str).is_none() {
            return Err(PortError::Git("worktree does not exist".into()));
        }

        Ok(())
    }

    async fn rebase(&self, branch: &str, onto: &str) -> Result<RebaseResult, PortError> {
        let mut inner = self.inner.write();

        if !inner.branches.contains_key(branch) {
            return Err(PortError::Git(format!(
                "branch '{}' does not exist",
                branch
            )));
        }
        if !inner.branches.contains_key(onto) {
            return Err(PortError::Git(format!("branch '{}' does not exist", onto)));
        }

        if let Some(ref merge) = inner.rebase_state {
            let conflict_files = merge.conflicting_files.clone();
            inner.rebase_state = None;

            let entries: Vec<ConflictEntry> = conflict_files
                .iter()
                .map(|p| ConflictEntry {
                    path: p.clone(),
                    conflict_type: ConflictType::BothModified,
                })
                .collect();

            let summary = format!(
                "Rebase of '{}' onto '{}' conflicted in {} file(s); \
                 fallback strategies exhausted.",
                branch,
                onto,
                conflict_files.len(),
            );

            return Ok(RebaseResult::Conflict {
                entries,
                fallback_attempted: RebaseFallbackStrategy::CherryPickRemaining,
                fallback_succeeded: None,
                summary,
                files: conflict_files,
            });
        }

        inner.current_branch = branch.to_string();
        Ok(RebaseResult::Success)
    }

    async fn merge(&self, branch: &str) -> Result<MergeResult, PortError> {
        let mut inner = self.inner.write();

        if !inner.branches.contains_key(branch) {
            return Err(PortError::Git(format!(
                "branch '{}' does not exist",
                branch
            )));
        }

        if let Some(ref merge) = inner.merge_state {
            let conflicts = merge.conflicting_files.clone();
            inner.merge_state = None;
            return Ok(MergeResult::Conflict { files: conflicts });
        }

        Ok(MergeResult::Success)
    }

    async fn diff(&self, branch: &str, base: &str) -> Result<Diff, PortError> {
        let inner = self.inner.read();

        let branch_state = inner
            .branches
            .get(branch)
            .ok_or_else(|| PortError::Git(format!("branch '{}' does not exist", branch)))?;
        let base_state = inner
            .branches
            .get(base)
            .ok_or_else(|| PortError::Git(format!("branch '{}' does not exist", base)))?;

        let mut files = Vec::new();

        for (path, content) in &branch_state.files {
            if let Some(base_content) = base_state.files.get(path) {
                if content.content != base_content.content {
                    files.push(FileDiff {
                        path: path.clone(),
                        additions: content.lines_added,
                        deletions: content.lines_removed,
                    });
                }
            } else {
                files.push(FileDiff {
                    path: path.clone(),
                    additions: content.lines_added,
                    deletions: 0,
                });
            }
        }

        for path in base_state.files.keys() {
            if !branch_state.files.contains_key(path) {
                files.push(FileDiff {
                    path: path.clone(),
                    additions: 0,
                    deletions: 10,
                });
            }
        }

        Ok(Diff { files })
    }

    async fn has_conflicts(&self, branch: &str, base: &str) -> Result<Vec<String>, PortError> {
        let inner = self.inner.read();

        let branch_state = inner
            .branches
            .get(branch)
            .ok_or_else(|| PortError::Git(format!("branch '{}' does not exist", branch)))?;
        let base_state = inner
            .branches
            .get(base)
            .ok_or_else(|| PortError::Git(format!("branch '{}' does not exist", base)))?;

        let mut conflicts = Vec::new();

        for path in branch_state.files.keys() {
            if base_state.files.contains_key(path) {
                conflicts.push(path.clone());
            }
        }

        Ok(conflicts)
    }

    async fn current_branch(&self) -> Result<String, PortError> {
        Ok(self.inner.read().current_branch.clone())
    }

    async fn checkout(&self, branch: &str) -> Result<(), PortError> {
        let mut inner = self.inner.write();

        if !inner.branches.contains_key(branch) {
            return Err(PortError::Git(format!(
                "branch '{}' does not exist",
                branch
            )));
        }

        inner.current_branch = branch.to_string();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_and_remove_worktree() {
        let git = FakeGitOperations::new();
        git.create_branch("feature");

        let path = std::path::Path::new("/tmp/worktree");

        git.create_worktree("feature", path).await.unwrap();

        git.remove_worktree(path).await.unwrap();
    }

    #[tokio::test]
    async fn rebase_success() {
        let git = FakeGitOperations::new();
        git.create_branch("feature");

        let result = git.rebase("feature", "main").await.unwrap();
        assert!(matches!(result, RebaseResult::Success));
    }

    #[tokio::test]
    async fn merge_success() {
        let git = FakeGitOperations::new();
        git.create_branch("feature");

        let result = git.merge("feature").await.unwrap();
        assert!(matches!(result, MergeResult::Success));
    }

    #[tokio::test]
    async fn current_branch() {
        let git = FakeGitOperations::new();

        let branch = git.current_branch().await.unwrap();
        assert_eq!(branch, "main");
    }

    #[tokio::test]
    async fn checkout() {
        let git = FakeGitOperations::new();
        git.create_branch("feature");

        git.checkout("feature").await.unwrap();

        let branch = git.current_branch().await.unwrap();
        assert_eq!(branch, "feature");
    }
}

//! Git metadata and gitignore helpers for Brehon.

use std::path::{Path, PathBuf};

use crate::error::GitError;

/// Worktree-aware gitignore patterns for the `.brehon` directory.
///
/// These patterns keep runtime/state files ignored while allowing worktree
/// directory paths to remain visible to git-aware CLIs such as Antigravity
/// CLI. Files inside those worktrees are still ignored from the shared
/// checkout, so the main repo does not become dirty.
///
/// The patterns use a root-relative glob (`!/.brehon/` etc.) because they are
/// intended for a `.gitignore` or `.git/info/exclude` at the repository root.
/// For nested directories (e.g. inside crate workspaces) a different glob
/// (`**/*/.brehon/`) is needed because gitignore rules are relative to the
/// file they appear in.
pub const WORKTREE_AWARE_BREHON_IGNORE_PATTERNS: &[&str] = &[
    "!/.brehon/",
    "/.brehon/*",
    "!/.brehon/worktrees/",
    "/.brehon/worktrees/**",
    "!/.brehon/worktrees/**/",
];

/// Resolve the actual `.git/info` directory for a repository root.
///
/// In a normal checkout this is simply `root/.git/info`, but in a linked
/// worktree `.git` is a file pointing at the real git directory, so we
/// must follow the `gitdir:` pointer and resolve `commondir` if present.
pub fn resolve_git_info_dir(root: &Path) -> Result<PathBuf, GitError> {
    let git_entry = root.join(".git");
    if git_entry.is_file() {
        let contents = std::fs::read_to_string(&git_entry).map_err(|err| {
            GitError::IoError(format!(
                "Failed to read .git file at '{}': {err}",
                git_entry.display()
            ))
        })?;
        let gitdir = contents
            .trim()
            .strip_prefix("gitdir:")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                GitError::IoError(format!(
                    ".git file at '{}' does not contain a valid gitdir pointer",
                    git_entry.display()
                ))
            })?;
        let gitdir_path = PathBuf::from(gitdir);
        let resolved = if gitdir_path.is_absolute() {
            gitdir_path
        } else {
            root.join(gitdir_path)
        };
        let commondir_line = std::fs::read_to_string(resolved.join("commondir"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let info_base = if let Some(ref common) = commondir_line {
            let common_path = PathBuf::from(common);
            let common_resolved = if common_path.is_absolute() {
                common_path
            } else {
                resolved.join(&common_path)
            };
            common_resolved.join("info")
        } else {
            resolved.join("info")
        };
        Ok(info_base)
    } else {
        Ok(git_entry.join("info"))
    }
}

/// Whether a gitignore/exclude line is a legacy blanket `.brehon` ignore
/// that should be migrated to worktree-aware patterns.
pub fn is_legacy_brehon_dir_ignore(line: &str) -> bool {
    matches!(line.trim(), ".brehon" | ".brehon/")
}

/// Strip legacy blanket `.brehon` ignores from gitignore/exclude content.
///
/// Returns the cleaned content and whether any legacy lines were removed.
pub fn remove_legacy_brehon_dir_ignores(content: &str) -> (String, bool) {
    let mut removed = false;
    let retained = content
        .lines()
        .filter(|line| {
            if is_legacy_brehon_dir_ignore(line) {
                removed = true;
                false
            } else {
                true
            }
        })
        .collect::<Vec<_>>();

    let mut updated = retained.join("\n");
    if !updated.is_empty() && content.ends_with('\n') {
        updated.push('\n');
    }
    (updated, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitignore_worktree_aware_patterns_has_five_entries() {
        assert_eq!(WORKTREE_AWARE_BREHON_IGNORE_PATTERNS.len(), 5);
        assert!(WORKTREE_AWARE_BREHON_IGNORE_PATTERNS.contains(&"!/.brehon/"));
        assert!(WORKTREE_AWARE_BREHON_IGNORE_PATTERNS.contains(&"/.brehon/*"));
        assert!(WORKTREE_AWARE_BREHON_IGNORE_PATTERNS.contains(&"!/.brehon/worktrees/"));
        assert!(WORKTREE_AWARE_BREHON_IGNORE_PATTERNS.contains(&"/.brehon/worktrees/**"));
        assert!(WORKTREE_AWARE_BREHON_IGNORE_PATTERNS.contains(&"!/.brehon/worktrees/**/"));
    }

    #[test]
    fn gitignore_legacy_dir_ignore_detection() {
        assert!(is_legacy_brehon_dir_ignore(".brehon"));
        assert!(is_legacy_brehon_dir_ignore(".brehon/"));
        assert!(is_legacy_brehon_dir_ignore("  .brehon  "));
        assert!(!is_legacy_brehon_dir_ignore("!/.brehon/"));
        assert!(!is_legacy_brehon_dir_ignore("/.brehon/*"));
        assert!(!is_legacy_brehon_dir_ignore(".brehon/config.yaml"));
    }

    #[test]
    fn gitignore_remove_legacy_ignores_strips_blanket_patterns() {
        let content = "target/\n.brehon\n*.log\n.brehon/\n";
        let (updated, removed) = remove_legacy_brehon_dir_ignores(content);
        assert!(removed);
        let lines: Vec<_> = updated.lines().collect();
        assert!(lines.contains(&"target/"));
        assert!(lines.contains(&"*.log"));
        assert!(!lines.iter().any(|l| l.trim() == ".brehon"));
        assert!(!lines.iter().any(|l| l.trim() == ".brehon/"));
    }

    #[test]
    fn gitignore_remove_legacy_ignores_preserves_trailing_newline() {
        let content = "target/\n.brehon\n";
        let (updated, removed) = remove_legacy_brehon_dir_ignores(content);
        assert!(removed);
        assert!(updated.ends_with('\n'));
    }

    #[test]
    fn gitignore_remove_legacy_ignores_noop_when_no_legacy() {
        let content = "target/\n!/.brehon/\n/.brehon/*\n";
        let (updated, removed) = remove_legacy_brehon_dir_ignores(content);
        assert!(!removed);
        assert_eq!(updated, content);
    }
}

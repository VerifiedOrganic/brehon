//! Git metadata and gitignore helpers for Brehon.

use std::path::{Path, PathBuf};

use crate::error::GitError;

/// Gitignore patterns for Brehon's machine-local `.brehon` directory.
///
/// The constant name is retained for API compatibility with callers that were
/// introduced during the Antigravity worktree experiment. The actual contract
/// is intentionally a blanket ignore: worktrees nested under `.brehon/` are
/// full checkouts with their own `.gitignore` files, and unignoring their
/// directories lets those nested rules re-expose files in the shared root.
pub const WORKTREE_AWARE_BREHON_IGNORE_PATTERNS: &[&str] = &[".brehon/"];

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

/// Whether a gitignore/exclude line is a legacy Brehon ignore pattern that
/// should be migrated to the current blanket `.brehon/` rule.
pub fn is_legacy_brehon_dir_ignore(line: &str) -> bool {
    matches!(
        line.trim(),
        ".brehon"
            | "!/.brehon/"
            | "!.brehon/"
            | "/.brehon/*"
            | ".brehon/*"
            | "!/.brehon/runtime/"
            | "!.brehon/runtime/"
            | "/.brehon/runtime/*"
            | ".brehon/runtime/*"
            | "!/.brehon/runtime/proof/"
            | "!.brehon/runtime/proof/"
            | "!/.brehon/runtime/proof/*.json"
            | "!.brehon/runtime/proof/*.json"
            | "!/.brehon/worktrees/"
            | "/.brehon/worktrees/**"
            | "!/.brehon/worktrees/**/"
    )
}

/// Strip legacy Brehon ignore patterns from gitignore/exclude content.
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
    fn gitignore_brehon_patterns_use_blanket_ignore() {
        assert_eq!(WORKTREE_AWARE_BREHON_IGNORE_PATTERNS, &[".brehon/"]);
    }

    #[test]
    fn gitignore_legacy_dir_ignore_detection() {
        assert!(is_legacy_brehon_dir_ignore(".brehon"));
        assert!(is_legacy_brehon_dir_ignore("!/.brehon/"));
        assert!(is_legacy_brehon_dir_ignore("!.brehon/"));
        assert!(is_legacy_brehon_dir_ignore("/.brehon/*"));
        assert!(is_legacy_brehon_dir_ignore(".brehon/*"));
        assert!(is_legacy_brehon_dir_ignore("!.brehon/runtime/"));
        assert!(is_legacy_brehon_dir_ignore(".brehon/runtime/*"));
        assert!(is_legacy_brehon_dir_ignore("!.brehon/runtime/proof/"));
        assert!(is_legacy_brehon_dir_ignore("!.brehon/runtime/proof/*.json"));
        assert!(is_legacy_brehon_dir_ignore("!/.brehon/worktrees/"));
        assert!(is_legacy_brehon_dir_ignore("/.brehon/worktrees/**"));
        assert!(is_legacy_brehon_dir_ignore("!/.brehon/worktrees/**/"));
        assert!(is_legacy_brehon_dir_ignore("  .brehon  "));
        assert!(!is_legacy_brehon_dir_ignore(".brehon/"));
        assert!(!is_legacy_brehon_dir_ignore(".brehon/config.yaml"));
    }

    #[test]
    fn gitignore_remove_legacy_ignores_strips_blanket_patterns() {
        let content = "target/\n.brehon\n*.log\n.brehon/\n!/.brehon/\n/.brehon/worktrees/**\n!.brehon/\n.brehon/*\n!.brehon/runtime/\n.brehon/runtime/*\n!.brehon/runtime/proof/\n!.brehon/runtime/proof/*.json\n";
        let (updated, removed) = remove_legacy_brehon_dir_ignores(content);
        assert!(removed);
        let lines: Vec<_> = updated.lines().collect();
        assert!(lines.contains(&"target/"));
        assert!(lines.contains(&"*.log"));
        assert!(lines.contains(&".brehon/"));
        assert!(!lines.iter().any(|l| l.trim() == ".brehon"));
        assert!(!lines.iter().any(|l| l.trim() == "!/.brehon/"));
        assert!(!lines.iter().any(|l| l.trim() == "/.brehon/worktrees/**"));
        assert!(!lines
            .iter()
            .any(|l| l.trim() == "!.brehon/runtime/proof/*.json"));
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
        let content = "target/\n.brehon/\n";
        let (updated, removed) = remove_legacy_brehon_dir_ignores(content);
        assert!(!removed);
        assert_eq!(updated, content);
    }
}

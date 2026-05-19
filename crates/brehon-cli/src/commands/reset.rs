//! `brehon reset` — clear runtime state and worktrees while preserving the
//! project's `.brehon/config.yaml` (and any other user-authored content such
//! as rules, skills, or memories).
//!
//! Motivated by a 2026-04-23 user report: after a finished run, brehon-owned
//! git worktrees were hanging around and cluttering the repo. `brehon clean`
//! nukes the entire `.brehon/` directory (including the config), so it's the
//! wrong tool for cleanup-between-runs. `brehon reset` is.
//!
//! ## Safety invariants
//!
//! 1. **Never touch `main`/`master`/`develop`/`trunk`/`HEAD`.** All branch
//!    deletions go through [`super::clean::delete_branch`], which calls
//!    [`super::clean::is_safe_brehon_branch`] and refuses anything outside
//!    the strict `brehon/<non-protected>` namespace.
//! 2. **Never touch the primary worktree.** The worktree removal path
//!    filters on the same safe-branch predicate and additionally refuses
//!    worktrees whose path equals the project root.
//! 3. **Never touch `.brehon/config.yaml`.** This module only deletes paths
//!    it explicitly allowlists (runtime/, worktrees/, brehon.log, *.log).
//! 4. **Never touch `.gitignore`.** Keeping the entries means `brehon run`
//!    after a reset does not re-pollute git status.
//!
//! If you extend this module, preserve all four.

use std::path::Path;

use anyhow::Result;

use crate::ui;

use super::clean::{delete_branch, is_git_repo, list_branches_with_prefix, remove_brehon_worktrees};

/// Allowlist of top-level entries inside `.brehon/` that `reset` is permitted
/// to delete. Anything not on this list is preserved — this is the contract
/// that keeps `config.yaml` and any user-authored rules/skills/memories safe
/// even if the layout grows.
const RESET_REMOVABLE_DIRS: &[&str] = &["runtime", "worktrees"];
const RESET_REMOVABLE_FILES: &[&str] = &["brehon.log"];

/// Remove a single path if it exists, reporting the outcome via the UI.
fn remove_path_if_exists(path: &Path, label: &str) {
    if !path.exists() {
        return;
    }
    let result = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match result {
        Ok(()) => ui::print_success(&format!("Removed {}", ui::dim(label))),
        Err(e) => ui::print_warning(&format!("Could not remove {}: {}", label, e)),
    }
}

/// Enumerate runtime artefacts that would be removed. Used for the dry-run
/// preview before the confirmation prompt.
fn enumerate_targets(brehon_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for dir in RESET_REMOVABLE_DIRS {
        if brehon_dir.join(dir).exists() {
            out.push(format!(".brehon/{}/", dir));
        }
    }
    for file in RESET_REMOVABLE_FILES {
        if brehon_dir.join(file).exists() {
            out.push(format!(".brehon/{}", file));
        }
    }
    // Catch any ad-hoc *.log files at the top level of .brehon/ without
    // descending into subdirectories we do not own.
    if let Ok(entries) = std::fs::read_dir(brehon_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && path.extension().is_some_and(|ext| ext == "log")
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| name != "brehon.log")
            {
                out.push(format!(
                    ".brehon/{}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
        }
    }
    out
}

pub fn execute(project_path: &Path, force: bool) -> Result<()> {
    let brehon_dir = project_path.join(".brehon");
    let config_path = brehon_dir.join("config.yaml");

    if !brehon_dir.exists() {
        println!();
        ui::print_warning("Nothing to reset — no .brehon/ directory found.");
        println!();
        return Ok(());
    }

    let in_git_repo = is_git_repo(project_path);
    let brehon_branches = if in_git_repo {
        list_branches_with_prefix(project_path, "brehon/")
    } else {
        vec![]
    };

    let removable_paths = enumerate_targets(&brehon_dir);

    if removable_paths.is_empty() && brehon_branches.is_empty() && !in_git_repo {
        println!();
        ui::print_warning(
            "Nothing to reset — no runtime state, worktrees, or brehon branches found.",
        );
        println!();
        return Ok(());
    }

    println!();
    ui::print_section("Brehon Reset");
    println!(
        "    {}",
        ui::dim("Preserves .brehon/config.yaml and any user-authored content.")
    );
    println!();

    if config_path.exists() {
        println!(
            "    {} {} {}",
            ui::dim("✓"),
            ui::dim("keep"),
            ui::dim(".brehon/config.yaml")
        );
    }
    for path in &removable_paths {
        println!("    {} {}", ui::dim("•"), ui::dim(path));
    }
    if in_git_repo {
        println!(
            "    {} {}",
            ui::dim("•"),
            ui::dim("brehon/* git worktrees (if any)")
        );
    }
    for branch in &brehon_branches {
        println!(
            "    {} {}",
            ui::dim("•"),
            ui::dim(&format!("branch {branch}"))
        );
    }
    println!();

    if !force {
        eprint!(
            "  {} Reset runtime state, worktrees, and brehon/* branches? [y/N] ",
            ui::yellow("?")
        );
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
            println!();
            ui::print_warning("Aborted.");
            println!();
            return Ok(());
        }
        println!();
    }

    // Worktrees first — same ordering as clean, since the branch deletion
    // below will refuse a branch that still has a checked-out worktree.
    if in_git_repo {
        match remove_brehon_worktrees(project_path) {
            Ok(removed) => {
                for name in &removed {
                    ui::print_success(&format!("Removed worktree {}", ui::dim(name)));
                }
            }
            Err(e) => ui::print_warning(&format!("Could not clean worktrees: {}", e)),
        }
    }

    // Branches — each individually gated by is_safe_brehon_branch inside
    // delete_branch, so even an unexpected name in the list cannot harm
    // main/master/etc.
    for branch in &brehon_branches {
        match delete_branch(project_path, branch) {
            Ok(()) => ui::print_success(&format!("Removed branch {}", ui::dim(branch))),
            Err(e) => ui::print_warning(&format!("Could not remove branch '{}': {}", branch, e)),
        }
    }

    // Allowlisted runtime dirs + files.
    for dir in RESET_REMOVABLE_DIRS {
        let p = brehon_dir.join(dir);
        remove_path_if_exists(&p, &format!(".brehon/{}/", dir));
    }
    for file in RESET_REMOVABLE_FILES {
        let p = brehon_dir.join(file);
        remove_path_if_exists(&p, &format!(".brehon/{}", file));
    }
    // Additional top-level *.log files surfaced during enumeration (not
    // recursed; we do not own subdirectories outside the allowlist).
    if let Ok(entries) = std::fs::read_dir(&brehon_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name == "brehon.log" {
                continue; // already handled above
            }
            if path.extension().is_some_and(|ext| ext == "log") {
                remove_path_if_exists(&path, &format!(".brehon/{}", name));
            }
        }
    }

    println!();
    ui::print_rule();
    println!();
    if config_path.exists() {
        println!(
            "    {}",
            ui::dim("Config preserved. Run 'brehon run' to start a fresh session.")
        );
    } else {
        println!(
            "    {}",
            ui::dim("No config.yaml found — run 'brehon init' to create one.")
        );
    }
    println!();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::clean::is_safe_brehon_branch;

    #[test]
    fn safe_branch_guard_accepts_normal_brehon_branches() {
        assert!(is_safe_brehon_branch("brehon/worker-1"));
        assert!(is_safe_brehon_branch("brehon/epic/T-abc123"));
        assert!(is_safe_brehon_branch("brehon/runs/session-a/worker-2"));
    }

    #[test]
    fn safe_branch_guard_refuses_main_and_variants() {
        for name in [
            "main",
            "master",
            "develop",
            "trunk",
            "HEAD",
            "brehon",
            "brehon/",
            "",
            "  ",
            "brehon/main",
            "brehon/MAIN",
            "brehon/master",
            "brehon/HEAD",
            "brehon/develop",
            "brehon/trunk",
            "brehon/sub/main",
            "refs/heads/main",
            "origin/main",
            "../main",
        ] {
            assert!(
                !is_safe_brehon_branch(name),
                "branch '{}' should be refused by safety guard",
                name
            );
        }
    }

    #[test]
    fn safe_branch_guard_refuses_branches_with_whitespace_or_dotdot() {
        assert!(!is_safe_brehon_branch("brehon/worker 1"));
        assert!(!is_safe_brehon_branch("brehon/..escape"));
        assert!(!is_safe_brehon_branch("brehon/a/../b"));
    }

    #[test]
    fn safe_branch_guard_tolerates_trailing_newline_from_porcelain() {
        assert!(is_safe_brehon_branch("brehon/worker-1\n"));
        assert!(!is_safe_brehon_branch("main\n"));
    }
}

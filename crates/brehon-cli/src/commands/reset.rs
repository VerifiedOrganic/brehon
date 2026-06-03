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
//! 2. **Never touch the primary worktree.** The worktree removal path refuses
//!    the project root. Scoped worker worktrees must pass the `brehon/*`
//!    branch predicate; integration worktrees must live under a Brehon-owned
//!    worktree root and use an `epic/*` or `initiative/*` branch.
//! 3. **Never touch `.brehon/config.yaml`.** This module only deletes paths
//!    it explicitly allowlists (runtime/, worktrees/, brehon.log, *.log).
//! 4. **Never touch `.gitignore`.** Keeping the entries means `brehon run`
//!    after a reset does not re-pollute git status.
//!
//! If you extend this module, preserve all four.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::ui;

use super::clean::{
    delete_branch, is_git_repo, list_branches_with_prefix, remove_brehon_worktrees,
};

/// Allowlist of top-level entries inside `.brehon/` that `reset` is permitted
/// to delete. Anything not on this list is preserved — this is the contract
/// that keeps `config.yaml` and any user-authored rules/skills/memories safe
/// even if the layout grows.
const RESET_REMOVABLE_DIRS: &[&str] = &["runtime", "worktrees"];
const RESET_REMOVABLE_FILES: &[&str] = &["brehon.log"];
const RESET_INTEGRATION_BRANCH_PREFIXES: &[&str] = &["epic/", "initiative/"];
const RESET_PROTECTED_BRANCH_COMPONENTS: &[&str] = &["main", "master", "develop", "trunk", "head"];

fn reset_worktree_roots(project_path: &Path, brehon_dir: &Path) -> Vec<PathBuf> {
    let project_root = brehon_dir
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(project_path);
    let project_root = super::run::normalize_project_root(project_root);
    let config = brehon_config::load_config(Some(&project_root)).unwrap_or_else(|_| {
        brehon_config::parse_defaults().expect("baked-in Brehon defaults should parse")
    });

    let mut roots = vec![
        super::run::effective_worktree_root(&project_root, &config),
        brehon_types::OrchestrationConfig::legacy_worktree_root(&project_root),
    ];
    let mut seen = HashSet::new();
    roots.retain(|root| {
        let key = root
            .canonicalize()
            .unwrap_or_else(|_| root.clone())
            .to_string_lossy()
            .to_string();
        seen.insert(key)
    });
    roots
}

fn is_safe_reset_integration_branch(branch: &str) -> bool {
    let branch = branch.trim();
    if branch.is_empty() || branch.contains("..") || branch.chars().any(char::is_whitespace) {
        return false;
    }
    if !RESET_INTEGRATION_BRANCH_PREFIXES
        .iter()
        .any(|prefix| branch.starts_with(prefix))
    {
        return false;
    }
    !branch
        .to_ascii_lowercase()
        .split('/')
        .any(|component| RESET_PROTECTED_BRANCH_COMPONENTS.contains(&component))
}

fn remove_reset_integration_worktrees(
    project_path: &Path,
    worktree_roots: &[PathBuf],
) -> Result<Vec<String>> {
    if worktree_roots.is_empty() {
        return Ok(Vec::new());
    }

    let output = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(project_path)
        .output();

    let output = match output {
        Ok(out) if out.status.success() => out,
        _ => return Ok(vec![]),
    };

    let canonical_project = project_path.canonicalize().ok();
    let canonical_roots = worktree_roots
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect::<Vec<_>>();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut removed = Vec::new();

    for block in stdout.split("\n\n") {
        let lines: Vec<&str> = block.lines().collect();
        let path = lines
            .iter()
            .find_map(|line| line.strip_prefix("worktree "))
            .unwrap_or("");
        let Some(branch_name) = lines
            .iter()
            .find_map(|line| line.strip_prefix("branch refs/heads/"))
        else {
            continue;
        };
        if !is_safe_reset_integration_branch(branch_name) {
            continue;
        }
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty() || path == project_path {
            continue;
        }
        let canonical_path = path.canonicalize().unwrap_or_else(|_| path.clone());
        if canonical_project.as_ref() == Some(&canonical_path) {
            continue;
        }
        if !canonical_roots
            .iter()
            .any(|root| canonical_path.starts_with(root))
        {
            continue;
        }

        let output = std::process::Command::new("git")
            .args(["worktree", "remove", "--force", &path.to_string_lossy()])
            .current_dir(project_path)
            .output()?;
        if output.status.success() {
            removed.push(branch_name.to_string());
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            ui::print_warning(&format!(
                "Could not remove integration worktree '{}': {}",
                path.display(),
                stderr.trim()
            ));
        }
    }

    Ok(removed)
}

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
    let worktree_roots = reset_worktree_roots(project_path, &brehon_dir);

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
        for root in &worktree_roots {
            if root != &brehon_dir.join("worktrees") && root.exists() {
                println!(
                    "    {} {}",
                    ui::dim("•"),
                    ui::dim(&format!(
                        "Brehon integration worktrees under {} (if any)",
                        root.display()
                    ))
                );
            }
        }
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
        match remove_reset_integration_worktrees(project_path, &worktree_roots) {
            Ok(removed) => {
                for name in &removed {
                    ui::print_success(&format!("Removed integration worktree {}", ui::dim(name)));
                }
            }
            Err(e) => ui::print_warning(&format!("Could not clean integration worktrees: {}", e)),
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

    // Remove the Brehon-installed Claude PreToolUse hook from
    // .claude/settings.local.json (and the runtime active marker).
    match super::run::remove_claude_worktree_hook(project_path) {
        Ok(()) => ui::print_success("Removed Claude worktree-containment hook"),
        Err(e) => ui::print_warning(&format!("Could not remove Claude worktree hook: {}", e)),
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
    use super::*;

    fn run_git(path: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(path)
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

    fn init_git_repo(path: &Path) {
        run_git(path, &["init", "-b", "main"]);
        run_git(path, &["config", "user.email", "brehon@example.invalid"]);
        run_git(path, &["config", "user.name", "Brehon Test"]);
        std::fs::write(path.join("README.md"), "seed\n").unwrap();
        run_git(path, &["add", "README.md"]);
        run_git(path, &["commit", "-m", "seed"]);
    }

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

    #[test]
    fn reset_removes_external_integration_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());

        let brehon_dir = temp.path().join(".brehon");
        std::fs::create_dir_all(&brehon_dir).unwrap();
        std::fs::write(
            brehon_dir.join("config.yaml"),
            format!(
                "orchestration:\n  worktree_root: {}\n",
                external.path().join("brehon-worktrees").display()
            ),
        )
        .unwrap();

        let worktree_path = external.path().join("brehon-worktrees/initiative/T-init");
        std::fs::create_dir_all(worktree_path.parent().unwrap()).unwrap();
        let worktree_arg = worktree_path.to_string_lossy().to_string();
        run_git(
            temp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "initiative/reset-test",
                &worktree_arg,
            ],
        );

        execute(temp.path(), true).unwrap();

        let worktree_list = run_git(temp.path(), &["worktree", "list", "--porcelain"]);
        assert!(
            !worktree_list.contains(&worktree_arg),
            "external integration worktree should be unregistered:\n{worktree_list}"
        );
        assert!(
            !worktree_path.exists(),
            "external integration worktree directory should be removed"
        );
        assert!(
            brehon_dir.join("config.yaml").exists(),
            "reset must preserve project config"
        );
    }
}

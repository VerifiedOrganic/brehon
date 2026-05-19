use std::path::Path;
use std::process::Command;

use anyhow::Result;

use crate::ui;

/// Remove the `.brehon/` directory and all its contents.
fn remove_brehon_dir(project_path: &Path) -> Result<bool> {
    let brehon_dir = project_path.join(".brehon");
    if brehon_dir.exists() {
        std::fs::remove_dir_all(&brehon_dir)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Remove brehon-related lines from `.gitignore`.
fn clean_gitignore(project_path: &Path) -> Result<bool> {
    let gitignore_path = project_path.join(".gitignore");
    if !gitignore_path.exists() {
        return Ok(false);
    }

    let content = std::fs::read_to_string(&gitignore_path)?;
    let filtered: Vec<&str> = content
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            trimmed != ".brehon/"
                && trimmed != ".brehon"
                && trimmed != "# Brehon orchestration data"
        })
        .collect();

    // Check if anything was removed
    if filtered.len() == content.lines().count() {
        return Ok(false);
    }

    // Trim trailing blank lines and ensure single trailing newline
    let mut result: Vec<&str> = filtered.to_vec();
    while result.last().is_some_and(|l| l.is_empty()) {
        result.pop();
    }

    if result.is_empty() {
        std::fs::remove_file(&gitignore_path)?;
    } else {
        let mut output = result.join("\n");
        output.push('\n');
        std::fs::write(&gitignore_path, output)?;
    }

    Ok(true)
}

/// List local git branches matching a prefix.
pub(crate) fn list_branches_with_prefix(project_path: &Path, prefix: &str) -> Vec<String> {
    let output = Command::new("git")
        .args(["branch", "--list", &format!("{prefix}*")])
        .current_dir(project_path)
        .output();

    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().trim_start_matches("* ").to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        _ => vec![],
    }
}

/// Names we will refuse to delete under any circumstances, even if somehow
/// someone names a branch `brehon/main` (it still trips the protected-name
/// check below). Keeps the blast radius of any prefix-match bug bounded.
const PROTECTED_BRANCH_NAMES: &[&str] = &[
    "main", "master", "develop", "trunk", "HEAD", "brehon", "brehon/", "",
];

/// Guard: every branch we consider for deletion must start with the
/// `brehon/` namespace AND must not match a protected name anywhere in its
/// components. We trim the name first so a stray trailing `\n` from porcelain
/// output can't sneak a hidden-character bypass past us.
pub(crate) fn is_safe_brehon_branch(name: &str) -> bool {
    let trimmed = name.trim();
    if !trimmed.starts_with("brehon/") {
        return false;
    }
    if PROTECTED_BRANCH_NAMES.contains(&trimmed) {
        return false;
    }
    if trimmed.contains("..") || trimmed.contains(char::is_whitespace) {
        return false;
    }
    // Treat every path component beyond `brehon/` as protected too — e.g.
    // refuse `brehon/main`, `brehon/master`, `brehon/HEAD` because those are
    // the shapes a typo could produce and still match the prefix.
    let tail = &trimmed["brehon/".len()..];
    if tail.is_empty() {
        return false;
    }
    for component in tail.split('/') {
        if PROTECTED_BRANCH_NAMES
            .iter()
            .any(|p| p.eq_ignore_ascii_case(component))
        {
            return false;
        }
    }
    true
}

/// Delete a local git branch.
///
/// Refuses to run against any name that does not pass [`is_safe_brehon_branch`].
/// This is the last line of defence against the branch-wipe incident class: a
/// caller can still pass whatever they want, but we will not forward it to
/// `git branch -D` unless the name is unambiguously an brehon-owned branch.
pub(crate) fn delete_branch(project_path: &Path, name: &str) -> Result<()> {
    if !is_safe_brehon_branch(name) {
        anyhow::bail!(
            "Refusing to delete branch '{}': only branches strictly under the `brehon/` namespace may be removed by this tool, and never protected names like main/master/develop/trunk/HEAD.",
            name
        );
    }
    let output = Command::new("git")
        .args(["branch", "-D", name])
        .current_dir(project_path)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to delete branch '{}': {}", name, stderr.trim());
    }
    Ok(())
}

/// List and remove git worktrees with brehon-prefixed names.
pub(crate) fn remove_brehon_worktrees(project_path: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(project_path)
        .output();

    let output = match output {
        Ok(out) if out.status.success() => out,
        _ => return Ok(vec![]),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut removed = Vec::new();

    // Porcelain format: blocks separated by blank lines, each block has
    // "worktree <path>\nHEAD <sha>\nbranch refs/heads/<name>\n"
    for block in stdout.split("\n\n") {
        let lines: Vec<&str> = block.lines().collect();
        let path = lines
            .iter()
            .find_map(|l| l.strip_prefix("worktree "))
            .unwrap_or("");
        let branch = lines
            .iter()
            .find_map(|l| l.strip_prefix("branch refs/heads/"));

        if let Some(branch_name) = branch {
            if !is_safe_brehon_branch(branch_name) {
                if branch_name.starts_with("brehon/") {
                    tracing::warn!(
                        "Refusing to remove worktree '{}' — branch '{}' did not pass the safe-brehon-branch guard",
                        path,
                        branch_name
                    );
                }
                continue;
            }
            // Defence in depth: the primary worktree also appears in porcelain
            // output; `git worktree remove` refuses to touch it, but we skip
            // it explicitly so no future git version could surprise us.
            if path.is_empty() || path == project_path.to_string_lossy() {
                tracing::warn!("Refusing to remove primary worktree at '{}'", path);
                continue;
            }
            let rm_output = Command::new("git")
                .args(["worktree", "remove", "--force", path])
                .current_dir(project_path)
                .output();

            match rm_output {
                Ok(out) if out.status.success() => {
                    removed.push(branch_name.to_string());
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    tracing::warn!("Could not remove worktree '{}': {}", path, stderr.trim());
                }
                Err(e) => {
                    tracing::warn!("Could not remove worktree '{}': {}", path, e);
                }
            }
        }
    }

    Ok(removed)
}

/// Check whether this project is inside a git repo.
pub(crate) fn is_git_repo(project_path: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(project_path)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn execute(project_path: &Path, force: bool) -> Result<()> {
    let brehon_dir = project_path.join(".brehon");
    let gitignore_path = project_path.join(".gitignore");

    let has_brehon_dir = brehon_dir.exists();
    let has_gitignore_entry = gitignore_path.exists()
        && std::fs::read_to_string(&gitignore_path)
            .map(|c| {
                c.lines()
                    .any(|l| l.trim() == ".brehon/" || l.trim() == ".brehon")
            })
            .unwrap_or(false);

    let in_git_repo = is_git_repo(project_path);
    let brehon_branches = if in_git_repo {
        list_branches_with_prefix(project_path, "brehon/")
    } else {
        vec![]
    };
    let has_protected_branch_hooks = in_git_repo
        && super::run::protected_branch_hooks_installed(project_path).unwrap_or_else(|err| {
            tracing::warn!("Could not inspect Brehon protected branch git hooks: {err}");
            false
        });

    if !has_brehon_dir
        && !has_gitignore_entry
        && brehon_branches.is_empty()
        && !has_protected_branch_hooks
    {
        println!();
        ui::print_warning("Nothing to clean — no brehon artifacts found.");
        println!();
        return Ok(());
    }

    // Show what will be removed
    println!();
    ui::print_section("Brehon Clean");

    if has_brehon_dir {
        println!("    {} {}", ui::dim("•"), ui::dim(".brehon/ directory"));
    }
    if has_gitignore_entry {
        println!(
            "    {} {}",
            ui::dim("•"),
            ui::dim(".brehon entries in .gitignore")
        );
    }
    if !brehon_branches.is_empty() {
        for branch in &brehon_branches {
            println!(
                "    {} {}",
                ui::dim("•"),
                ui::dim(&format!("branch {branch}"))
            );
        }
    }
    if in_git_repo {
        println!(
            "    {} {}",
            ui::dim("•"),
            ui::dim("brehon/* git worktrees (if any)")
        );
    }
    if has_protected_branch_hooks {
        println!(
            "    {} {}",
            ui::dim("•"),
            ui::dim("protected branch git hook guards")
        );
    }
    println!();

    // Confirm unless --force
    if !force {
        eprint!("  {} Remove all brehon artifacts? [y/N] ", ui::yellow("?"));
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

    // Remove worktrees first (before branches, since worktrees reference them)
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

    // Remove Brehon's injected guard blocks while preserving any pre-existing
    // user hook body that shared the same hook files.
    if in_git_repo {
        match super::run::remove_protected_branch_hooks(project_path) {
            Ok(removed) => {
                for hook in &removed {
                    ui::print_success(&format!(
                        "Removed protected branch guard from {}",
                        ui::dim(&hook.display().to_string())
                    ));
                }
            }
            Err(e) => ui::print_warning(&format!(
                "Could not clean protected branch git hooks: {}",
                e
            )),
        }
    }

    // Remove the Brehon-installed Claude PreToolUse hook from
    // .claude/settings.local.json (and the runtime active marker).
    match super::run::remove_claude_worktree_hook(project_path) {
        Ok(()) => ui::print_success("Removed Claude worktree-containment hook"),
        Err(e) => ui::print_warning(&format!("Could not remove Claude worktree hook: {}", e)),
    }

    // Remove branches
    for branch in &brehon_branches {
        match delete_branch(project_path, branch) {
            Ok(()) => ui::print_success(&format!("Removed branch {}", ui::dim(branch))),
            Err(e) => ui::print_warning(&format!("Could not remove branch '{}': {}", branch, e)),
        }
    }

    // Remove .brehon/ directory
    match remove_brehon_dir(project_path) {
        Ok(true) => ui::print_success(&format!("Removed {}", ui::dim(".brehon/"))),
        Ok(false) => {}
        Err(e) => ui::print_warning(&format!("Could not remove .brehon/: {}", e)),
    }

    // Clean .gitignore
    match clean_gitignore(project_path) {
        Ok(true) => ui::print_success(&format!(
            "Cleaned brehon entries from {}",
            ui::dim(".gitignore")
        )),
        Ok(false) => {}
        Err(e) => ui::print_warning(&format!("Could not clean .gitignore: {}", e)),
    }

    println!();
    ui::print_rule();
    println!();
    println!(
        "    {}",
        ui::dim("Run 'brehon init' to re-initialize the project.")
    );
    println!();

    Ok(())
}

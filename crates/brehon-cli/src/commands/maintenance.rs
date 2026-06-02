//! `brehon maintenance` — non-destructive reporting and optional pruning of stale
//! Brehon worktrees, run branches, and failed-import branches.
//!
//! ## Safety invariants
//!
//! 1. **Report-only by default.** Without `--prune`, this command never mutates
//!    git state.
//! 2. **Explicit confirmation required for deletion.** `--prune` prints a preview
//!    and requires a `y/yes` response before deleting anything.
//! 3. **Never delete protected branches.** Branch deletions use the
//!    `is_safe_maintenance_branch` guard, which accepts `brehon/`, `epic/`,
//!    and `initiative/` prefixes (plus the configured `branch_prefix`) while
//!    rejecting protected names like main, master, develop, trunk, and HEAD.
//! 4. **Never remove the primary worktree or the current working directory.**
//!    Worktree removal skips the project root and the CWD.
//! 5. **Distinguishes active from stale.** Branches and worktrees tied to the
//!    current run session or non-terminal tasks are reported as *active* and
//!    are never pruned.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde_json::Value;

use super::run::effective_worktree_root;
use crate::ui;

/// Parsed state of a single Brehon task loaded from `.brehon/runtime/tasks/`.
#[derive(Debug, Clone)]
struct TaskState {
    id: String,
    #[allow(dead_code)]
    status: String,
    merge_target: Option<String>,
    epic_branch: Option<String>,
    integration_branch: Option<String>,
    is_terminal: bool,
}

/// Current runtime session extracted from `current-session.json`.
#[derive(Debug, Clone)]
struct CurrentSession {
    session_name: String,
}

/// A branch or worktree categorized as active or stale.
#[derive(Debug, Clone)]
enum RuntimeState {
    Active { reason: String },
    Stale { reason: String },
}

/// A single item in the maintenance report.
#[derive(Debug, Clone)]
struct ReportItem {
    name: String,
    path: Option<PathBuf>,
    state: RuntimeState,
    category: String,
}

/// Complete maintenance report.
#[derive(Debug, Clone, Default)]
struct MaintenanceReport {
    items: Vec<ReportItem>,
}

impl MaintenanceReport {
    fn active_items(&self) -> Vec<&ReportItem> {
        self.items
            .iter()
            .filter(|item| matches!(item.state, RuntimeState::Active { .. }))
            .collect()
    }

    fn stale_items(&self) -> Vec<&ReportItem> {
        self.items
            .iter()
            .filter(|item| matches!(item.state, RuntimeState::Stale { .. }))
            .collect()
    }

    fn stale_branches(&self) -> Vec<&ReportItem> {
        self.items
            .iter()
            .filter(|item| {
                matches!(item.state, RuntimeState::Stale { .. }) && item.category.contains("branch")
            })
            .collect()
    }

    fn stale_worktrees(&self) -> Vec<&ReportItem> {
        self.items
            .iter()
            .filter(|item| {
                matches!(item.state, RuntimeState::Stale { .. })
                    && item.category.contains("worktree")
            })
            .collect()
    }
}

// ── Runtime state loading ───────────────────────────────────────────────────

fn load_current_session(brehon_dir: &Path) -> Option<CurrentSession> {
    let path = brehon_dir.join("runtime").join("current-session.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let json: Value = serde_json::from_str(&content).ok()?;
    let session_name = json.get("session_name")?.as_str()?.to_string();
    Some(CurrentSession { session_name })
}

fn load_task_states(brehon_dir: &Path) -> Result<Vec<TaskState>> {
    let tasks_dir = brehon_dir.join("runtime").join("tasks");
    if !tasks_dir.exists() {
        return Ok(Vec::new());
    }

    let mut states = Vec::new();
    for entry in std::fs::read_dir(&tasks_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read task file {}", path.display()))?;
            let json: Value = serde_json::from_str(&content)
                .with_context(|| format!("Failed to parse task JSON {}", path.display()))?;

            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let status = json
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let merge_target = json
                .get("merge_target")
                .and_then(|v| v.as_str())
                .map(String::from);
            let epic_branch = json
                .get("integration")
                .and_then(|v| v.as_object())
                .and_then(|o| o.get("epic_branch"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let integration_branch = json
                .get("integration_branch")
                .and_then(|v| v.as_str())
                .map(String::from);

            let is_terminal = brehon_types::is_terminal_task_status(&status);

            states.push(TaskState {
                id,
                status,
                merge_target,
                epic_branch,
                integration_branch,
                is_terminal,
            });
        }
    }
    Ok(states)
}

// ── Git helpers ─────────────────────────────────────────────────────────────

/// Find the `.brehon` directory that contains runtime state.
///
/// When invoked from a worktree, `project_path` points at the worktree root,
/// which may have its own `.brehon` (e.g. factory-runtime) but not the main
/// runtime state. We fall back to the primary worktree (first entry from
/// `git worktree list`) when the local `.brehon` lacks a `runtime/` directory
/// or a `worktrees/` directory.
fn resolve_brehon_dir(project_path: &Path) -> Option<PathBuf> {
    let local = project_path.join(".brehon");
    if local.join("config.yaml").is_file()
        || local.join("runtime").is_dir()
        || local.join("worktrees").is_dir()
    {
        return Some(local);
    }

    // Try to locate the primary worktree via git.
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(project_path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_path = stdout
        .split("\n\n")
        .next()?
        .lines()
        .find_map(|l| l.strip_prefix("worktree "))?;

    let candidate = PathBuf::from(first_path).join(".brehon");
    if candidate.join("config.yaml").is_file()
        || candidate.join("runtime").is_dir()
        || candidate.join("worktrees").is_dir()
    {
        Some(candidate)
    } else {
        None
    }
}

fn brehon_worktree_roots(
    project_path: &Path,
    brehon_dir: &Path,
    config: &brehon_types::BrehonConfig,
) -> Vec<PathBuf> {
    let project_root = brehon_dir
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(project_path);
    let project_root = super::run::normalize_project_root(project_root);
    let roots = vec![
        effective_worktree_root(&project_root, config),
        brehon_types::OrchestrationConfig::legacy_worktree_root(&project_root),
    ];

    let mut seen = HashSet::new();
    roots
        .into_iter()
        .filter(|root| {
            let key = root
                .canonicalize()
                .unwrap_or_else(|_| root.clone())
                .to_string_lossy()
                .to_string();
            seen.insert(key)
        })
        .collect()
}

/// Normalize a branch prefix so it matches the rules used by `worker_branch_name()`.
///
/// - Trims whitespace.
/// - If empty, returns empty.
/// - Otherwise ensures exactly one trailing `/` (e.g. `"brehon"` → `"brehon/"`,
///   `"acme/team/"` → `"acme/team/"`).
pub(crate) fn normalize_branch_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim();
    if trimmed.is_empty() {
        "".to_string()
    } else {
        format!("{}/", trimmed.trim_end_matches('/'))
    }
}

fn current_git_branch(project_path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(project_path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        return None;
    }
    Some(branch)
}

/// Return the path of the primary (first) worktree from git porcelain.
fn primary_worktree_path(project_path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(project_path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .split("\n\n")
        .next()?
        .lines()
        .find_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
}

/// Parse `git worktree list --porcelain` into (path, branch_name) pairs.
fn list_worktrees(project_path: &Path) -> Vec<(PathBuf, String)> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(project_path)
        .output();

    let output = match output {
        Ok(out) if out.status.success() => out,
        _ => return vec![],
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut result = Vec::new();

    for block in stdout.split("\n\n") {
        let lines: Vec<&str> = block.lines().collect();
        let path = lines
            .iter()
            .find_map(|l| l.strip_prefix("worktree "))
            .unwrap_or("");
        let branch = lines
            .iter()
            .find_map(|l| l.strip_prefix("branch refs/heads/"));
        let detached = lines.iter().any(|l| l.trim() == "detached");
        let head_sha = lines
            .iter()
            .find_map(|l| l.strip_prefix("HEAD "))
            .unwrap_or("");

        if !path.is_empty() {
            if let Some(branch_name) = branch {
                result.push((PathBuf::from(path), branch_name.to_string()));
            } else if detached {
                // Use the directory name or HEAD SHA as a display name for
                // detached-HEAD worktrees so they are not silently excluded.
                let display_name = if !head_sha.is_empty() {
                    format!("detached@{}", &head_sha[..head_sha.len().min(8)])
                } else {
                    PathBuf::from(path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "detached".to_string())
                };
                result.push((PathBuf::from(path), display_name));
            }
        }
    }

    result
}

// ── Categorization logic ────────────────────────────────────────────────────

fn analyze_branches(
    project_path: &Path,
    session: &Option<CurrentSession>,
    tasks: &[TaskState],
    configured_prefix: &str,
) -> Vec<ReportItem> {
    let mut items = Vec::new();
    let current_branch = current_git_branch(project_path);

    // Build lookup sets for active state.
    let active_merge_targets: HashSet<&str> = tasks
        .iter()
        .filter(|t| !t.is_terminal)
        .filter_map(|t| t.merge_target.as_deref())
        .collect();
    let active_epic_branches: HashSet<&str> = tasks
        .iter()
        .filter(|t| !t.is_terminal)
        .filter_map(|t| t.epic_branch.as_deref())
        .collect();
    let active_integration_branches: HashSet<&str> = tasks
        .iter()
        .filter(|t| !t.is_terminal)
        .filter_map(|t| t.integration_branch.as_deref())
        .collect();
    let current_session_name = session.as_ref().map(|s| s.session_name.as_str());

    // Collect all relevant branch prefixes.
    // Use a BTreeSet so that if configured_prefix happens to equal "epic/"
    // or "initiative/" we do not enumerate the same branch twice, and so
    // iteration order is deterministic.
    let mut all_branches = BTreeSet::new();
    if configured_prefix.is_empty() {
        // Empty-prefix mode: only enumerate explicit Brehon shapes so we do
        // not pull in every local branch via `git branch --list "*"`.
        all_branches.extend(super::clean::list_branches_with_prefix(
            project_path,
            "runs/",
        ));
        all_branches.extend(super::clean::list_branches_with_prefix(
            project_path,
            "archive/",
        ));
    } else {
        all_branches.extend(super::clean::list_branches_with_prefix(
            project_path,
            configured_prefix,
        ));
    }
    all_branches.extend(super::clean::list_branches_with_prefix(
        project_path,
        "epic/",
    ));
    all_branches.extend(super::clean::list_branches_with_prefix(
        project_path,
        "initiative/",
    ));

    for branch in all_branches {
        let is_current = current_branch.as_ref() == Some(&branch);

        // Strip the configured prefix so runs/<session>/... parsing is
        // independent of prefix depth (e.g. "brehon/" vs "acme/team/").
        let relative = if let Some(stripped) = branch.strip_prefix(configured_prefix) {
            stripped
        } else {
            branch.as_str()
        };

        let (state, category) = if relative.starts_with("runs/") {
            // Extract session name: runs/<session>/...
            let parts: Vec<&str> = relative.split('/').collect();
            let branch_session = parts.get(1).copied();
            let is_active_session = branch_session == current_session_name;

            if is_current {
                (
                    RuntimeState::Active {
                        reason: "currently checked out".to_string(),
                    },
                    "run branch",
                )
            } else if is_active_session {
                (
                    RuntimeState::Active {
                        reason: format!(
                            "part of current session '{}'",
                            branch_session.unwrap_or("")
                        ),
                    },
                    "run branch",
                )
            } else {
                (
                    RuntimeState::Stale {
                        reason: format!(
                            "session '{}' is not the current session",
                            branch_session.unwrap_or("unknown")
                        ),
                    },
                    "stale run branch",
                )
            }
        } else if relative.starts_with("archive/") {
            (
                RuntimeState::Stale {
                    reason: "archive branch".to_string(),
                },
                "archive branch",
            )
        } else if branch.starts_with("epic/") {
            if active_merge_targets.contains(branch.as_str())
                || active_epic_branches.contains(branch.as_str())
                || active_integration_branches.contains(branch.as_str())
            {
                (
                    RuntimeState::Active {
                        reason: "associated with active task".to_string(),
                    },
                    "epic branch",
                )
            } else if is_current {
                (
                    RuntimeState::Active {
                        reason: "currently checked out".to_string(),
                    },
                    "epic branch",
                )
            } else {
                (
                    RuntimeState::Stale {
                        reason: "no active task targets this epic".to_string(),
                    },
                    "stale epic branch",
                )
            }
        } else if branch.starts_with("initiative/") {
            if active_merge_targets.contains(branch.as_str())
                || active_integration_branches.contains(branch.as_str())
            {
                (
                    RuntimeState::Active {
                        reason: "associated with active task".to_string(),
                    },
                    "initiative branch",
                )
            } else if is_current {
                (
                    RuntimeState::Active {
                        reason: "currently checked out".to_string(),
                    },
                    "initiative branch",
                )
            } else {
                (
                    RuntimeState::Stale {
                        reason: "no active task targets this initiative".to_string(),
                    },
                    "stale initiative branch",
                )
            }
        } else if !configured_prefix.is_empty() && branch.starts_with(configured_prefix) {
            // Catch-all for other configured-prefix branches (e.g. failed-import leftovers).
            if is_current {
                (
                    RuntimeState::Active {
                        reason: "currently checked out".to_string(),
                    },
                    "run branch",
                )
            } else {
                (
                    RuntimeState::Stale {
                        reason: format!(
                            "unmatched {} branch (possible failed-import leftover)",
                            configured_prefix.trim_end_matches('/')
                        ),
                    },
                    "failed-import branch",
                )
            }
        } else {
            continue;
        };

        items.push(ReportItem {
            name: branch,
            path: None,
            state,
            category: category.to_string(),
        });
    }

    items
}

fn analyze_worktrees(
    project_path: &Path,
    worktree_roots: &[PathBuf],
    session: &Option<CurrentSession>,
    tasks: &[TaskState],
) -> Vec<ReportItem> {
    let mut items = Vec::new();
    let worktrees = list_worktrees(project_path);
    let current_dir = std::env::current_dir().ok();
    let canonical_cwd = current_dir.as_ref().and_then(|p| p.canonicalize().ok());
    let canonical_primary = primary_worktree_path(project_path).and_then(|p| p.canonicalize().ok());

    let active_task_ids: HashSet<&str> = tasks
        .iter()
        .filter(|t| !t.is_terminal)
        .map(|t| t.id.as_str())
        .collect();
    let current_session_name = session.as_ref().map(|s| s.session_name.as_str());

    // Only consider worktrees under Brehon-owned roots. This includes the
    // effective external root and the legacy in-repo `.brehon/worktrees`
    // root so old runs remain visible and prunable.
    let canonical_roots = worktree_roots
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect::<Vec<_>>();

    for (path, branch) in worktrees {
        // Skip the primary worktree. Use the resolved primary checkout rather
        // than project_path so the caller's own linked worktree is not dropped
        // when the command is run from inside a worktree.
        if canonical_primary.as_ref() == path.canonicalize().ok().as_ref() {
            continue;
        }

        let canonical_path = path.canonicalize().unwrap_or_else(|_| path.clone());
        let Some(canonical_root) = canonical_roots
            .iter()
            .find(|root| canonical_path.starts_with(root))
        else {
            continue;
        };

        let is_cwd = canonical_cwd
            .as_ref()
            .map(|cwd| cwd.starts_with(&canonical_path))
            .unwrap_or(false);

        let relative = match canonical_path.strip_prefix(canonical_root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let relative_str = relative.to_string_lossy();

        let (state, category) = if relative_str.starts_with("runs/") {
            // runs/<session>/<agent>
            let parts: Vec<&str> = relative_str.split('/').collect();
            let wt_session = parts.get(1).copied();
            let is_active_session = wt_session == current_session_name;

            if is_cwd {
                (
                    RuntimeState::Active {
                        reason: "current working directory".to_string(),
                    },
                    "run worktree",
                )
            } else if is_active_session {
                (
                    RuntimeState::Active {
                        reason: format!("part of current session '{}'", wt_session.unwrap_or("")),
                    },
                    "run worktree",
                )
            } else {
                (
                    RuntimeState::Stale {
                        reason: format!(
                            "session '{}' is not the current session",
                            wt_session.unwrap_or("unknown")
                        ),
                    },
                    "stale run worktree",
                )
            }
        } else if relative_str.starts_with("epic/") {
            // epic/T-<taskid>
            let parts: Vec<&str> = relative_str.split('/').collect();
            let task_id = parts.get(1).copied().unwrap_or("");
            let is_active = active_task_ids.contains(task_id);

            if is_cwd {
                (
                    RuntimeState::Active {
                        reason: "current working directory".to_string(),
                    },
                    "epic worktree",
                )
            } else if is_active {
                (
                    RuntimeState::Active {
                        reason: format!("epic task {task_id} is active"),
                    },
                    "epic worktree",
                )
            } else {
                (
                    RuntimeState::Stale {
                        reason: format!("epic task {task_id} is terminal or missing"),
                    },
                    "stale epic worktree",
                )
            }
        } else if relative_str.starts_with("initiative/") {
            // initiative/T-<taskid>
            let parts: Vec<&str> = relative_str.split('/').collect();
            let task_id = parts.get(1).copied().unwrap_or("");
            let is_active = active_task_ids.contains(task_id);

            if is_cwd {
                (
                    RuntimeState::Active {
                        reason: "current working directory".to_string(),
                    },
                    "initiative worktree",
                )
            } else if is_active {
                (
                    RuntimeState::Active {
                        reason: format!("initiative task {task_id} is active"),
                    },
                    "initiative worktree",
                )
            } else {
                (
                    RuntimeState::Stale {
                        reason: format!("initiative task {task_id} is terminal or missing"),
                    },
                    "stale initiative worktree",
                )
            }
        } else if relative_str.starts_with("_archived") {
            (
                RuntimeState::Stale {
                    reason: "archived worktree".to_string(),
                },
                "archived worktree",
            )
        } else {
            if is_cwd {
                (
                    RuntimeState::Active {
                        reason: "current working directory".to_string(),
                    },
                    "worktree",
                )
            } else {
                (
                    RuntimeState::Stale {
                        reason: "unmatched worktree".to_string(),
                    },
                    "stale worktree",
                )
            }
        };

        items.push(ReportItem {
            name: branch,
            path: Some(path),
            state,
            category: category.to_string(),
        });
    }

    items
}

// ── Reporting ───────────────────────────────────────────────────────────────

fn print_report(report: &MaintenanceReport) {
    let active = report.active_items();
    let stale = report.stale_items();

    println!();
    ui::print_section("Brehon Maintenance Report");
    println!();

    if active.is_empty() && stale.is_empty() {
        ui::print_success("No Brehon-managed branches or worktrees found.");
        println!();
        return;
    }

    if !active.is_empty() {
        println!("  {}", ui::bold_green("Active runtime state"));
        for item in &active {
            let reason = match &item.state {
                RuntimeState::Active { reason } => reason,
                _ => continue,
            };
            println!(
                "    {} {} {} {}",
                ui::green("✓"),
                ui::dim(&item.category),
                ui::dim(&item.name),
                ui::dim(&format!("({})", reason))
            );
        }
        println!();
    }

    if !stale.is_empty() {
        println!("  {}", ui::bold("Stale / prunable leftovers"));
        for item in &stale {
            let reason = match &item.state {
                RuntimeState::Stale { reason } => reason,
                _ => continue,
            };
            println!(
                "    {} {} {} {}",
                ui::yellow("•"),
                ui::dim(&item.category),
                ui::dim(&item.name),
                ui::dim(&format!("({})", reason))
            );
        }
        println!();
    }

    let stale_branch_count = report.stale_branches().len();
    let stale_worktree_count = report.stale_worktrees().len();

    if stale_branch_count > 0 || stale_worktree_count > 0 {
        println!(
            "    {} stale branch(es), {} stale worktree(s) could be pruned.",
            stale_branch_count, stale_worktree_count
        );
        println!();
        println!(
            "    {}",
            ui::dim("Run with --prune to remove stale items after confirmation.")
        );
    } else {
        println!(
            "    {}",
            ui::dim("All Brehon-managed branches and worktrees are active. Nothing to prune.")
        );
    }
    println!();
}

fn report_to_json(report: &MaintenanceReport) -> Value {
    let active: Vec<Value> = report
        .active_items()
        .iter()
        .map(|item| {
            serde_json::json!({
                "name": item.name,
                "path": item.path.as_ref().map(|p| p.to_string_lossy().to_string()),
                "category": item.category,
                "reason": match &item.state {
                    RuntimeState::Active { reason } => reason,
                    _ => "",
                }
            })
        })
        .collect();

    let stale: Vec<Value> = report
        .stale_items()
        .iter()
        .map(|item| {
            serde_json::json!({
                "name": item.name,
                "path": item.path.as_ref().map(|p| p.to_string_lossy().to_string()),
                "category": item.category,
                "reason": match &item.state {
                    RuntimeState::Stale { reason } => reason,
                    _ => "",
                }
            })
        })
        .collect();

    serde_json::json!({
        "active": active,
        "stale": stale,
        "summary": {
            "active_count": active.len(),
            "stale_count": stale.len(),
            "stale_branch_count": report.stale_branches().len(),
            "stale_worktree_count": report.stale_worktrees().len(),
        }
    })
}

// ── Safe deletion guards (maintenance-specific) ─────────────────────────────

const FIXED_MAINTENANCE_PREFIXES: &[&str] = &["epic/", "initiative/"];

const PROTECTED_BRANCH_NAMES_MAINTENANCE: &[&str] = &[
    "main",
    "master",
    "develop",
    "trunk",
    "HEAD",
    "brehon",
    "epic",
    "initiative",
    "",
];

/// Guard: every branch we consider for deletion in maintenance must start with
/// the configured prefix or one of the fixed integration prefixes, and must not
/// match a protected name anywhere in its components.
pub(crate) fn is_safe_maintenance_branch(name: &str, configured_prefix: &str) -> bool {
    let trimmed = name.trim();

    // Empty-prefix mode only recognizes explicit Brehon branch shapes.
    if configured_prefix.is_empty() {
        let shape_prefix = if trimmed.starts_with("runs/") {
            "runs/"
        } else if trimmed.starts_with("archive/") {
            "archive/"
        } else if trimmed.starts_with("epic/") {
            "epic/"
        } else if trimmed.starts_with("initiative/") {
            "initiative/"
        } else {
            return false;
        };
        let tail = &trimmed[shape_prefix.len()..];
        if tail.is_empty() {
            return false;
        }
        for component in tail.split('/') {
            if PROTECTED_BRANCH_NAMES_MAINTENANCE
                .iter()
                .any(|p| p.eq_ignore_ascii_case(component))
            {
                return false;
            }
        }
    } else {
        // Determine which prefix matched.
        let matched_prefix = if trimmed.starts_with(configured_prefix) {
            Some(configured_prefix)
        } else {
            FIXED_MAINTENANCE_PREFIXES
                .iter()
                .find(|p| trimmed.starts_with(**p))
                .copied()
        };

        let prefix = match matched_prefix {
            Some(p) => p,
            None => return false,
        };

        let tail = &trimmed[prefix.len()..];
        if tail.is_empty() {
            return false;
        }
        for component in tail.split('/') {
            if PROTECTED_BRANCH_NAMES_MAINTENANCE
                .iter()
                .any(|p| p.eq_ignore_ascii_case(component))
            {
                return false;
            }
        }
    }

    if PROTECTED_BRANCH_NAMES_MAINTENANCE.contains(&trimmed) {
        return false;
    }
    if trimmed.contains("..") || trimmed.contains(char::is_whitespace) {
        return false;
    }
    true
}

/// Delete a local git branch from the maintenance command.
///
/// Refuses to run against any name that does not pass [`is_safe_maintenance_branch`].
fn delete_branch_maintenance(
    project_path: &Path,
    name: &str,
    configured_prefix: &str,
) -> Result<()> {
    if !is_safe_maintenance_branch(name, configured_prefix) {
        anyhow::bail!(
            "Refusing to delete branch '{}': only branches under the configured prefix ('{}'), epic/, or initiative/ namespaces may be removed by this tool, and never protected names like main/master/develop/trunk/HEAD.",
            name, configured_prefix
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

// ── Pruning ─────────────────────────────────────────────────────────────────

fn prune_stale_items(
    project_path: &Path,
    report: &MaintenanceReport,
    force: bool,
    configured_prefix: &str,
) -> Result<()> {
    let stale_branches = report.stale_branches();
    let stale_worktrees = report.stale_worktrees();

    if stale_branches.is_empty() && stale_worktrees.is_empty() {
        println!();
        ui::print_success("Nothing to prune — no stale items found.");
        println!();
        return Ok(());
    }

    println!();
    ui::print_section("Brehon Maintenance Prune");
    println!();

    for item in &stale_branches {
        println!(
            "    {} {} {}",
            ui::dim("•"),
            ui::dim(&item.category),
            ui::dim(&item.name)
        );
    }
    for item in &stale_worktrees {
        println!(
            "    {} {} {} {}",
            ui::dim("•"),
            ui::dim(&item.category),
            ui::dim(&item.name),
            ui::dim(
                &item
                    .path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            )
        );
    }
    println!();

    if !force {
        eprint!(
            "  {} Remove stale branches and worktrees? [y/N] ",
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

    // Remove stale worktrees first (they hold branches).
    let canonical_primary = primary_worktree_path(project_path).and_then(|p| p.canonicalize().ok());
    let canonical_cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.canonicalize().ok());
    for item in &stale_worktrees {
        if let Some(path) = &item.path {
            // Skip CWD and primary worktree as defense in depth.
            let canonical_path = path.canonicalize().ok();

            if canonical_path
                .as_ref()
                .zip(canonical_cwd.as_ref())
                .map(|(path, cwd)| cwd.starts_with(path))
                .unwrap_or(false)
            {
                ui::print_warning(&format!(
                    "Skipping worktree '{}' — it is the current directory",
                    path.display()
                ));
                continue;
            }
            if canonical_path.as_ref() == canonical_primary.as_ref() {
                ui::print_warning(&format!(
                    "Skipping worktree '{}' — it is the primary worktree",
                    path.display()
                ));
                continue;
            }

            match remove_worktree(project_path, path, force) {
                Ok(()) => ui::print_success(&format!(
                    "Removed worktree {} {}",
                    ui::dim(&item.name),
                    ui::dim(&format!("at {}", path.display()))
                )),
                Err(e) => ui::print_warning(&format!(
                    "Could not remove worktree '{}': {}",
                    path.display(),
                    e
                )),
            }
        }
    }

    // Remove stale branches.
    for item in &stale_branches {
        // Use the maintenance-specific guard that accepts epic/ and initiative/.
        match delete_branch_maintenance(project_path, &item.name, configured_prefix) {
            Ok(()) => ui::print_success(&format!("Removed branch {}", ui::dim(&item.name))),
            Err(e) => ui::print_warning(&format!("Could not remove branch '{}': {}", item.name, e)),
        }
    }

    println!();
    ui::print_rule();
    println!();

    Ok(())
}

/// Remove a single worktree.
///
/// Tries without `--force` first so git's dirty-worktree safety check is
/// respected. When `force` is `true`, falls back to `--force` on any failure.
fn remove_worktree(project_path: &Path, path: &Path, force: bool) -> Result<()> {
    let output = Command::new("git")
        .args(["worktree", "remove", &path.to_string_lossy()])
        .current_dir(project_path)
        .output()?;

    if output.status.success() {
        return Ok(());
    }

    if force {
        let output = Command::new("git")
            .args(["worktree", "remove", "--force", &path.to_string_lossy()])
            .current_dir(project_path)
            .output()?;

        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree remove --force failed: {}", stderr.trim());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!("git worktree remove failed: {}", stderr.trim());
}

// ── Public entry point ──────────────────────────────────────────────────────

pub fn execute(project_path: &Path, prune: bool, force: bool, json: bool) -> Result<()> {
    let brehon_dir = match resolve_brehon_dir(project_path) {
        Some(dir) => dir,
        None => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "error": "No .brehon/ directory with runtime state found."
                    }))?
                );
            } else {
                println!();
                ui::print_warning(
                    "Nothing to report — no .brehon/ directory with runtime state found.",
                );
                println!();
            }
            return Ok(());
        }
    };

    // Load config to discover the configured branch prefix and effective
    // external worktree root.
    let config_project_path = brehon_dir
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(project_path);
    let config = brehon_config::load_config(Some(config_project_path)).unwrap_or_else(|_| {
        let mut config = brehon_config::parse_defaults()
            .expect("baked-in Brehon defaults should parse for maintenance fallback");
        config.orchestration.branch_prefix = "brehon/".to_string();
        config
    });
    let normalized_prefix = normalize_branch_prefix(&config.orchestration.branch_prefix);
    let worktree_roots = brehon_worktree_roots(project_path, &brehon_dir, &config);

    let session = load_current_session(&brehon_dir);
    let tasks = load_task_states(&brehon_dir)?;

    let mut report = MaintenanceReport::default();
    report.items.extend(analyze_branches(
        project_path,
        &session,
        &tasks,
        &normalized_prefix,
    ));
    report.items.extend(analyze_worktrees(
        project_path,
        &worktree_roots,
        &session,
        &tasks,
    ));

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report_to_json(&report))?
        );
        return Ok(());
    }

    print_report(&report);

    if prune {
        prune_stale_items(project_path, &report, force, &normalized_prefix)?;
    }

    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    // Tests that change the process CWD must hold this lock so they do not
    // race with each other when running in parallel.
    static MAINTENANCE_TEST_CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct CwdGuard {
        original: PathBuf,
    }

    impl CwdGuard {
        fn new(path: &std::path::Path) -> Self {
            let original = std::env::current_dir().unwrap();
            std::env::set_current_dir(path).unwrap();
            Self { original }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    #[test]
    fn load_task_states_skips_terminal_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let tasks_dir = temp.path().join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let active_task = serde_json::json!({
            "status": "in_progress",
            "merge_target": "epic/foo"
        });
        std::fs::write(tasks_dir.join("T-active.json"), active_task.to_string()).unwrap();

        let closed_task = serde_json::json!({
            "status": "closed",
            "merge_target": "epic/bar"
        });
        std::fs::write(tasks_dir.join("T-closed.json"), closed_task.to_string()).unwrap();

        let states = load_task_states(temp.path()).unwrap();
        assert_eq!(states.len(), 2);
        let active = states.iter().find(|t| t.id == "T-active").unwrap();
        assert!(!active.is_terminal);
        let closed = states.iter().find(|t| t.id == "T-closed").unwrap();
        assert!(closed.is_terminal);
    }

    #[test]
    fn report_summarizes_active_and_stale() {
        let mut report = MaintenanceReport::default();
        report.items.push(ReportItem {
            name: "brehon/runs/session-a/worker-1".to_string(),
            path: None,
            state: RuntimeState::Active {
                reason: "current session".to_string(),
            },
            category: "run branch".to_string(),
        });
        report.items.push(ReportItem {
            name: "brehon/runs/session-b/worker-1".to_string(),
            path: None,
            state: RuntimeState::Stale {
                reason: "old session".to_string(),
            },
            category: "stale run branch".to_string(),
        });

        assert_eq!(report.active_items().len(), 1);
        assert_eq!(report.stale_items().len(), 1);
        assert_eq!(report.stale_branches().len(), 1);
        assert_eq!(report.stale_worktrees().len(), 0);
    }

    #[test]
    fn report_to_json_structured_output() {
        let mut report = MaintenanceReport::default();
        report.items.push(ReportItem {
            name: "epic/foo".to_string(),
            path: None,
            state: RuntimeState::Stale {
                reason: "no active task".to_string(),
            },
            category: "stale epic branch".to_string(),
        });

        let json = report_to_json(&report);
        assert_eq!(json["active"].as_array().unwrap().len(), 0);
        assert_eq!(json["stale"].as_array().unwrap().len(), 1);
        assert_eq!(json["summary"]["stale_branch_count"], 1);
        assert_eq!(json["summary"]["stale_worktree_count"], 0);
    }

    #[test]
    fn is_safe_maintenance_branch_accepts_managed_prefixes_and_rejects_protected_names() {
        assert!(is_safe_maintenance_branch(
            "brehon/runs/session-a/agent-1",
            "brehon/"
        ));
        assert!(is_safe_maintenance_branch("epic/foo", "brehon/"));
        assert!(is_safe_maintenance_branch("initiative/bar", "brehon/"));
        assert!(is_safe_maintenance_branch("brehon/archive/old", "brehon/"));

        assert!(!is_safe_maintenance_branch("main", "brehon/"));
        assert!(!is_safe_maintenance_branch("master", "brehon/"));
        assert!(!is_safe_maintenance_branch("develop", "brehon/"));
        assert!(!is_safe_maintenance_branch("trunk", "brehon/"));
        assert!(!is_safe_maintenance_branch("HEAD", "brehon/"));
        assert!(!is_safe_maintenance_branch("brehon", "brehon/"));
        assert!(!is_safe_maintenance_branch("epic", "brehon/"));
        assert!(!is_safe_maintenance_branch("initiative", "brehon/"));
        assert!(!is_safe_maintenance_branch("brehon/main", "brehon/"));
        assert!(!is_safe_maintenance_branch("epic/master", "brehon/"));
        assert!(!is_safe_maintenance_branch("initiative/HEAD", "brehon/"));
        assert!(!is_safe_maintenance_branch("feature/foo", "brehon/"));
        assert!(!is_safe_maintenance_branch("brehon/../main", "brehon/"));
        assert!(!is_safe_maintenance_branch("brehon/foo bar", "brehon/"));
    }

    #[test]
    fn analyze_branches_categorizes_epic_and_initiative_branches() {
        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path();

        // Initialise a git repo so we can create branches.
        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(project_path)
                .status()
                .expect("git command failed");
            assert!(status.success(), "git {:?} failed", args);
        };

        run_git(&["init", "--quiet"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);

        // Create an initial commit so branches can be created.
        std::fs::write(project_path.join("file.txt"), "hello").unwrap();
        run_git(&["add", "file.txt"]);
        run_git(&["commit", "--quiet", "-m", "initial"]);

        // Create the branches we want to analyse.
        run_git(&["branch", "epic/foo"]);
        run_git(&["branch", "epic/bar"]);
        run_git(&["branch", "initiative/baz"]);
        run_git(&["branch", "brehon/runs/session-x/agent-1"]);

        // Simulate runtime state: epic/foo and initiative/baz are active;
        // epic/bar is terminal; session-y is current.
        let tasks = vec![
            TaskState {
                id: "T-active".to_string(),
                status: "in_progress".to_string(),
                merge_target: Some("epic/foo".to_string()),
                epic_branch: Some("epic/foo".to_string()),
                integration_branch: None,
                is_terminal: false,
            },
            TaskState {
                id: "T-closed".to_string(),
                status: "closed".to_string(),
                merge_target: Some("epic/bar".to_string()),
                epic_branch: Some("epic/bar".to_string()),
                integration_branch: None,
                is_terminal: true,
            },
            TaskState {
                id: "T-init".to_string(),
                status: "in_progress".to_string(),
                merge_target: Some("initiative/baz".to_string()),
                epic_branch: None,
                integration_branch: None,
                is_terminal: false,
            },
        ];

        let session = Some(CurrentSession {
            session_name: "session-y".to_string(),
        });

        let items = analyze_branches(project_path, &session, &tasks, "brehon/");

        let find = |name: &str| items.iter().find(|i| i.name == name);

        // Active branches.
        let epic_foo = find("epic/foo").expect("epic/foo should be present");
        assert!(
            matches!(&epic_foo.state,
                RuntimeState::Active { reason } if reason.contains("active task")
            ),
            "epic/foo should be active, got {:?}",
            epic_foo.state
        );

        let init_baz = find("initiative/baz").expect("initiative/baz should be present");
        assert!(
            matches!(&init_baz.state,
                RuntimeState::Active { reason } if reason.contains("active task")
            ),
            "initiative/baz should be active, got {:?}",
            init_baz.state
        );

        // Stale branches.
        let epic_bar = find("epic/bar").expect("epic/bar should be present");
        assert!(
            matches!(&epic_bar.state,
                RuntimeState::Stale { reason } if reason.contains("no active task")
            ),
            "epic/bar should be stale, got {:?}",
            epic_bar.state
        );

        let run_branch = find("brehon/runs/session-x/agent-1")
            .expect("brehon/runs/session-x/agent-1 should be present");
        assert!(
            matches!(&run_branch.state,
                RuntimeState::Stale { reason } if reason.contains("not the current session")
            ),
            "run branch should be stale, got {:?}",
            run_branch.state
        );
    }

    #[test]
    fn analyze_branches_deduplicates_when_configured_prefix_is_epic() {
        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path();

        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(project_path)
                .status()
                .expect("git command failed");
            assert!(status.success(), "git {:?} failed", args);
        };

        run_git(&["init", "--quiet"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);
        run_git(&["config", "commit.gpgsign", "false"]);

        std::fs::write(project_path.join("file.txt"), "hello").unwrap();
        run_git(&["add", "file.txt"]);
        run_git(&["commit", "--quiet", "-m", "initial"]);

        // Create an epic branch.
        run_git(&["branch", "epic/foo"]);

        // When configured_prefix is "epic/", the branch would be collected
        // twice without deduplication — once via the configured-prefix path
        // and once via the fixed epic/ enumeration.
        let items = analyze_branches(project_path, &None, &[], "epic/");

        let epic_items: Vec<_> = items.iter().filter(|i| i.name == "epic/foo").collect();
        assert_eq!(
            epic_items.len(),
            1,
            "epic/foo should appear exactly once, got: {:?}",
            epic_items
        );
    }

    #[test]
    fn delete_branch_maintenance_deletes_epic_and_initiative_branches() {
        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path();

        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(project_path)
                .status()
                .expect("git command failed");
            assert!(status.success(), "git {:?} failed", args);
        };

        run_git(&["init", "--quiet"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);

        std::fs::write(project_path.join("file.txt"), "hello").unwrap();
        run_git(&["add", "file.txt"]);
        run_git(&["commit", "--quiet", "-m", "initial"]);

        // Create branches under all three supported namespaces.
        run_git(&["branch", "epic/foo"]);
        run_git(&["branch", "initiative/bar"]);
        run_git(&["branch", "brehon/runs/session-a/agent-1"]);

        // Verify deletion succeeds for each namespace.
        delete_branch_maintenance(project_path, "epic/foo", "brehon/").unwrap();
        delete_branch_maintenance(project_path, "initiative/bar", "brehon/").unwrap();
        delete_branch_maintenance(project_path, "brehon/runs/session-a/agent-1", "brehon/")
            .unwrap();

        // Verify the branches are actually gone.
        let output = std::process::Command::new("git")
            .args(["branch", "--list"])
            .current_dir(project_path)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("epic/foo"));
        assert!(!stdout.contains("initiative/bar"));
        assert!(!stdout.contains("brehon/runs/session-a/agent-1"));

        // Verify protected names are still refused.
        assert!(
            delete_branch_maintenance(project_path, "main", "brehon/").is_err(),
            "should refuse to delete main"
        );
        assert!(
            delete_branch_maintenance(project_path, "epic/main", "brehon/").is_err(),
            "should refuse to delete epic/main"
        );
    }

    #[test]
    fn remove_worktree_respects_dirty_state_without_force() {
        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path();

        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(project_path)
                .status()
                .expect("git command failed");
            assert!(status.success(), "git {:?} failed", args);
        };

        run_git(&["init", "--quiet"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);

        std::fs::write(project_path.join("file.txt"), "hello").unwrap();
        run_git(&["add", "file.txt"]);
        run_git(&["commit", "--quiet", "-m", "initial"]);

        // Create a worktree.
        let worktree_path = project_path.join("wt");
        run_git(&[
            "worktree",
            "add",
            "--quiet",
            worktree_path.to_str().unwrap(),
            "HEAD",
        ]);

        // Add an uncommitted file to the worktree so it becomes dirty.
        std::fs::write(worktree_path.join("dirty.txt"), "dirty").unwrap();

        // Without force, removal should fail because the worktree is dirty.
        let result = remove_worktree(project_path, &worktree_path, false);
        assert!(
            result.is_err(),
            "remove_worktree without force should fail on dirty worktree"
        );
        // The worktree should still exist.
        assert!(worktree_path.exists());

        // With force, removal should succeed.
        remove_worktree(project_path, &worktree_path, true).unwrap();
        assert!(
            !worktree_path.exists(),
            "worktree should be removed with force"
        );
    }

    #[test]
    fn load_task_states_reads_integration_branch() {
        let temp = tempfile::tempdir().unwrap();
        let tasks_dir = temp.path().join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let task = serde_json::json!({
            "status": "in_progress",
            "integration_branch": "initiative/container-a"
        });
        std::fs::write(tasks_dir.join("T-container.json"), task.to_string()).unwrap();

        let states = load_task_states(temp.path()).unwrap();
        assert_eq!(states.len(), 1);
        let state = states.first().unwrap();
        assert_eq!(
            state.integration_branch,
            Some("initiative/container-a".to_string())
        );
    }

    #[test]
    fn analyze_branches_respects_integration_branch_for_active_container_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path();

        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(project_path)
                .status()
                .expect("git command failed");
            assert!(status.success(), "git {:?} failed", args);
        };

        run_git(&["init", "--quiet"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);

        std::fs::write(project_path.join("file.txt"), "hello").unwrap();
        run_git(&["add", "file.txt"]);
        run_git(&["commit", "--quiet", "-m", "initial"]);

        // epic/container has no active child tasks (merge_target empty) but the
        // container task itself carries integration_branch.
        run_git(&["branch", "epic/container"]);
        run_git(&["branch", "initiative/container"]);

        let tasks = vec![TaskState {
            id: "T-epic".to_string(),
            status: "in_progress".to_string(),
            merge_target: None,
            epic_branch: None,
            integration_branch: Some("epic/container".to_string()),
            is_terminal: false,
        }];

        let items = analyze_branches(project_path, &None, &tasks, "brehon/");

        let epic = items
            .iter()
            .find(|i| i.name == "epic/container")
            .expect("epic/container should be present");
        assert!(
            matches!(
                &epic.state,
                RuntimeState::Active { reason } if reason.contains("active task")
            ),
            "epic/container should be active via integration_branch, got {:?}",
            epic.state
        );

        // initiative/container is NOT targeted by integration_branch, so stale.
        let init = items
            .iter()
            .find(|i| i.name == "initiative/container")
            .expect("initiative/container should be present");
        assert!(
            matches!(
                &init.state,
                RuntimeState::Stale { reason } if reason.contains("no active task")
            ),
            "initiative/container should be stale, got {:?}",
            init.state
        );
    }

    #[test]
    fn list_worktrees_includes_detached_head() {
        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path();

        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(project_path)
                .status()
                .expect("git command failed");
            assert!(status.success(), "git {:?} failed", args);
        };

        run_git(&["init", "--quiet"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);

        std::fs::write(project_path.join("file.txt"), "hello").unwrap();
        run_git(&["add", "file.txt"]);
        run_git(&["commit", "--quiet", "-m", "initial"]);

        // Create a detached-HEAD worktree.
        let wt_path = project_path.join("detached-wt");
        run_git(&[
            "worktree",
            "add",
            "--detach",
            "--quiet",
            wt_path.to_str().unwrap(),
            "HEAD",
        ]);

        let worktrees = list_worktrees(project_path);
        let names: Vec<&str> = worktrees.iter().map(|(_, b)| b.as_str()).collect();
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("detached@") || *n == "detached-wt"),
            "detached-HEAD worktree should appear in list, got: {:?}",
            names
        );
    }

    #[test]
    fn analyze_worktrees_from_linked_worktree_reports_linked_cwd_active() {
        let _lock = MAINTENANCE_TEST_CWD_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path();

        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(project_path)
                .status()
                .expect("git command failed");
            assert!(status.success(), "git {:?} failed", args);
        };

        run_git(&["init", "--quiet"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);

        std::fs::write(project_path.join("file.txt"), "hello").unwrap();
        run_git(&["add", "file.txt"]);
        run_git(&["commit", "--quiet", "-m", "initial"]);

        // Create a linked worktree under .brehon/worktrees/linked
        let linked_path = project_path
            .join(".brehon")
            .join("worktrees")
            .join("linked");
        // Ensure parent exists but leaf does not, so git worktree add succeeds.
        std::fs::create_dir_all(linked_path.parent().unwrap()).unwrap();
        run_git(&[
            "worktree",
            "add",
            "--quiet",
            linked_path.to_str().unwrap(),
            "HEAD",
        ]);

        // Change CWD into the linked worktree so the command sees it as active.
        let _cwd_guard = CwdGuard::new(&linked_path);

        // Simulate runtime state: no current session, no active tasks.
        let roots = vec![project_path.join(".brehon").join("worktrees")];
        let items = analyze_worktrees(project_path, &roots, &None, &[]);

        // The linked worktree should be reported as Active because it is the CWD.
        let linked_item = items
            .iter()
            .find(|i| {
                i.path
                    .as_ref()
                    .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
                    == linked_path.canonicalize().ok()
            })
            .expect("linked worktree should appear in report");
        assert!(
            matches!(
                &linked_item.state,
                RuntimeState::Active { reason } if reason.contains("current working directory")
            ),
            "linked worktree should be active (current directory), got {:?}",
            linked_item.state
        );
    }

    #[test]
    fn analyze_worktrees_reports_external_root_worktrees() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let project_path = temp.path();

        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(project_path)
                .status()
                .expect("git command failed");
            assert!(status.success(), "git {:?} failed", args);
        };

        run_git(&["init", "--quiet"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);

        std::fs::write(project_path.join("file.txt"), "hello").unwrap();
        run_git(&["add", "file.txt"]);
        run_git(&["commit", "--quiet", "-m", "initial"]);

        let external_root = external.path().join("brehon-worktrees");
        let external_path = external_root.join("runs/old-session/worker-1");
        std::fs::create_dir_all(external_path.parent().unwrap()).unwrap();
        run_git(&[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "brehon/runs/old-session/worker-1",
            external_path.to_str().unwrap(),
            "HEAD",
        ]);

        let roots = vec![external_root];
        let items = analyze_worktrees(project_path, &roots, &None, &[]);

        let external_item = items
            .iter()
            .find(|i| {
                i.path
                    .as_ref()
                    .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
                    == external_path.canonicalize().ok()
            })
            .expect("external Brehon worktree should appear in report");
        assert!(
            matches!(
                &external_item.state,
                RuntimeState::Stale { reason } if reason.contains("old-session")
            ),
            "external worktree should be stale by session, got {:?}",
            external_item.state
        );
    }

    #[test]
    fn analyze_branches_uses_configured_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path();

        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(project_path)
                .status()
                .expect("git command failed");
            assert!(status.success(), "git {:?} failed", args);
        };

        run_git(&["init", "--quiet"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);

        std::fs::write(project_path.join("file.txt"), "hello").unwrap();
        run_git(&["add", "file.txt"]);
        run_git(&["commit", "--quiet", "-m", "initial"]);

        // Create branches under a custom prefix.
        run_git(&["branch", "custom/runs/session-a/agent-1"]);
        run_git(&["branch", "custom/archive/old"]);

        let items = analyze_branches(project_path, &None, &[], "custom/");

        let run = items
            .iter()
            .find(|i| i.name == "custom/runs/session-a/agent-1")
            .expect("custom run branch should be present");
        assert!(
            matches!(&run.state, RuntimeState::Stale { reason } if reason.contains("not the current session")
            ),
            "custom run branch should be stale, got {:?}",
            run.state
        );

        let archive = items
            .iter()
            .find(|i| i.name == "custom/archive/old")
            .expect("custom archive branch should be present");
        assert!(
            matches!(&archive.state, RuntimeState::Stale { reason } if reason.contains("archive")
            ),
            "custom archive branch should be stale, got {:?}",
            archive.state
        );
    }

    #[test]
    fn is_safe_maintenance_branch_respects_configured_prefix() {
        // Default prefix "brehon/"
        assert!(is_safe_maintenance_branch("brehon/worker-1", "brehon/"));
        assert!(!is_safe_maintenance_branch("custom/worker-1", "brehon/"));

        // Custom prefix "custom/"
        assert!(is_safe_maintenance_branch("custom/worker-1", "custom/"));
        assert!(!is_safe_maintenance_branch("brehon/worker-1", "custom/"));

        // epic/ and initiative/ are always allowed regardless of configured prefix.
        assert!(is_safe_maintenance_branch("epic/foo", "custom/"));
        assert!(is_safe_maintenance_branch("initiative/bar", "custom/"));
    }

    #[test]
    fn resolve_brehon_dir_finds_runtime_without_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_dir = temp.path().join(".brehon");
        let runtime_dir = brehon_dir.join("runtime");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::write(
            runtime_dir.join("current-session.json"),
            r#"{"session_name":"test-session"}"#,
        )
        .unwrap();

        assert_eq!(resolve_brehon_dir(temp.path()), Some(brehon_dir));
    }

    #[test]
    fn resolve_brehon_dir_finds_worktrees_without_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_dir = temp.path().join(".brehon");
        let worktrees_dir = brehon_dir.join("worktrees");
        std::fs::create_dir_all(&worktrees_dir).unwrap();

        assert_eq!(resolve_brehon_dir(temp.path()), Some(brehon_dir));
    }

    #[test]
    fn normalize_branch_prefix_covers_all_shapes() {
        assert_eq!(normalize_branch_prefix("brehon"), "brehon/");
        assert_eq!(normalize_branch_prefix("brehon/"), "brehon/");
        assert_eq!(normalize_branch_prefix("acme/team"), "acme/team/");
        assert_eq!(normalize_branch_prefix("acme/team/"), "acme/team/");
        assert_eq!(normalize_branch_prefix("  brehon  "), "brehon/");
        assert_eq!(normalize_branch_prefix(""), "");
        assert_eq!(normalize_branch_prefix("  "), "");
    }

    #[test]
    fn analyze_branches_multi_segment_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path();

        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(project_path)
                .status()
                .expect("git command failed");
            assert!(status.success(), "git {:?} failed", args);
        };

        run_git(&["init", "--quiet"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);

        std::fs::write(project_path.join("file.txt"), "hello").unwrap();
        run_git(&["add", "file.txt"]);
        run_git(&["commit", "--quiet", "-m", "initial"]);

        run_git(&["branch", "acme/team/runs/session-a/agent-1"]);
        run_git(&["branch", "acme/team/archive/old"]);
        run_git(&["branch", "acme/team/failed-import-xyz"]);

        let session = Some(CurrentSession {
            session_name: "session-b".to_string(),
        });

        let items = analyze_branches(project_path, &session, &[], "acme/team/");

        let run = items
            .iter()
            .find(|i| i.name == "acme/team/runs/session-a/agent-1")
            .expect("run branch should be present");
        assert!(
            matches!(
                &run.state,
                RuntimeState::Stale { reason } if reason.contains("not the current session")
            ),
            "run branch should be stale, got {:?}",
            run.state
        );

        let archive = items
            .iter()
            .find(|i| i.name == "acme/team/archive/old")
            .expect("archive branch should be present");
        assert!(
            matches!(
                &archive.state,
                RuntimeState::Stale { reason } if reason.contains("archive")
            ),
            "archive branch should be stale, got {:?}",
            archive.state
        );

        let failed = items
            .iter()
            .find(|i| i.name == "acme/team/failed-import-xyz")
            .expect("failed-import branch should be present");
        assert!(
            matches!(
                &failed.state,
                RuntimeState::Stale { reason } if reason.contains("failed-import")
            ),
            "failed-import branch should be stale, got {:?}",
            failed.state
        );
    }

    #[test]
    fn analyze_branches_no_trailing_slash_prefix_gets_normalized() {
        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path();

        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(project_path)
                .status()
                .expect("git command failed");
            assert!(status.success(), "git {:?} failed", args);
        };

        run_git(&["init", "--quiet"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);

        std::fs::write(project_path.join("file.txt"), "hello").unwrap();
        run_git(&["add", "file.txt"]);
        run_git(&["commit", "--quiet", "-m", "initial"]);

        run_git(&["branch", "acme/runs/session-a/agent-1"]);

        let session = Some(CurrentSession {
            session_name: "session-b".to_string(),
        });

        // Pass the *normalized* prefix, simulating what execute() does.
        let items = analyze_branches(project_path, &session, &[], "acme/");

        let run = items
            .iter()
            .find(|i| i.name == "acme/runs/session-a/agent-1")
            .expect("run branch should be present");
        assert!(
            matches!(
                &run.state,
                RuntimeState::Stale { reason } if reason.contains("not the current session")
            ),
            "run branch should be stale with normalized prefix, got {:?}",
            run.state
        );
    }

    #[test]
    fn analyze_branches_empty_prefix_only_recognizes_explicit_shapes() {
        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path();

        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(project_path)
                .status()
                .expect("git command failed");
            assert!(status.success(), "git {:?} failed", args);
        };

        run_git(&["init", "--quiet"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);

        std::fs::write(project_path.join("file.txt"), "hello").unwrap();
        run_git(&["add", "file.txt"]);
        run_git(&["commit", "--quiet", "-m", "initial"]);

        // Create a mix of Brehon-shaped and unrelated branches.
        run_git(&["branch", "runs/session-a/agent-1"]);
        run_git(&["branch", "archive/old"]);
        run_git(&["branch", "epic/foo"]);
        run_git(&["branch", "feature/user-thing"]);

        let session = Some(CurrentSession {
            session_name: "session-b".to_string(),
        });

        let items = analyze_branches(project_path, &session, &[], "");
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();

        // Explicit Brehon shapes are reported.
        assert!(
            names.contains(&"runs/session-a/agent-1"),
            "runs branch should be reported"
        );
        assert!(
            names.contains(&"archive/old"),
            "archive branch should be reported"
        );
        assert!(
            names.contains(&"epic/foo"),
            "epic branch should be reported"
        );

        // Unrelated branches must NOT leak into the report.
        assert!(
            !names.contains(&"feature/user-thing"),
            "unrelated feature branch should NOT be reported with empty prefix"
        );
    }

    #[test]
    fn is_safe_maintenance_branch_rejects_non_brehon_branches_with_empty_prefix() {
        // Empty-prefix mode only allows explicit Brehon shapes.
        assert!(is_safe_maintenance_branch("runs/session-a/agent-1", ""));
        assert!(is_safe_maintenance_branch("archive/old", ""));
        assert!(is_safe_maintenance_branch("epic/foo", ""));
        assert!(is_safe_maintenance_branch("initiative/bar", ""));

        assert!(!is_safe_maintenance_branch("feature/foo", ""));
        assert!(!is_safe_maintenance_branch("main", ""));
        assert!(!is_safe_maintenance_branch("master", ""));
        assert!(!is_safe_maintenance_branch("user-branch", ""));
        assert!(!is_safe_maintenance_branch("develop", ""));
    }

    #[test]
    fn delete_branch_maintenance_empty_prefix_refuses_non_brehon_branch() {
        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path();

        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(project_path)
                .status()
                .expect("git command failed");
            assert!(status.success(), "git {:?} failed", args);
        };

        run_git(&["init", "--quiet"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);

        std::fs::write(project_path.join("file.txt"), "hello").unwrap();
        run_git(&["add", "file.txt"]);
        run_git(&["commit", "--quiet", "-m", "initial"]);

        run_git(&["branch", "feature/foo"]);

        // With empty prefix, a non-Brehon branch must be refused for deletion.
        assert!(
            delete_branch_maintenance(project_path, "feature/foo", "").is_err(),
            "empty-prefix mode should refuse to delete feature/foo"
        );

        // A Brehon-shaped branch should succeed.
        run_git(&["branch", "runs/session-a/agent-1"]);
        delete_branch_maintenance(project_path, "runs/session-a/agent-1", "").unwrap();
        let output = std::process::Command::new("git")
            .args(["branch", "--list"])
            .current_dir(project_path)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("runs/session-a/agent-1"));
    }

    #[test]
    fn analyze_worktrees_from_nested_subdirectory_reports_worktree_active() {
        let _lock = MAINTENANCE_TEST_CWD_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path();

        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(project_path)
                .status()
                .expect("git command failed");
            assert!(status.success(), "git {:?} failed", args);
        };

        run_git(&["init", "--quiet"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);

        std::fs::write(project_path.join("file.txt"), "hello").unwrap();
        run_git(&["add", "file.txt"]);
        run_git(&["commit", "--quiet", "-m", "initial"]);

        // Create a linked worktree under .brehon/worktrees/linked
        let linked_path = project_path
            .join(".brehon")
            .join("worktrees")
            .join("linked");
        std::fs::create_dir_all(linked_path.parent().unwrap()).unwrap();
        run_git(&[
            "worktree",
            "add",
            "--quiet",
            linked_path.to_str().unwrap(),
            "HEAD",
        ]);

        // Create a nested subdirectory inside the linked worktree.
        let nested_path = linked_path.join("src").join("deep");
        std::fs::create_dir_all(&nested_path).unwrap();

        // Change CWD into the nested subdirectory.
        let _cwd_guard = CwdGuard::new(&nested_path);

        let roots = vec![project_path.join(".brehon").join("worktrees")];
        let items = analyze_worktrees(project_path, &roots, &None, &[]);

        // The linked worktree should still be reported as Active because the
        // CWD is a subdirectory inside it.
        let linked_item = items
            .iter()
            .find(|i| {
                i.path
                    .as_ref()
                    .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
                    == linked_path.canonicalize().ok()
            })
            .expect("linked worktree should appear in report");
        assert!(
            matches!(
                &linked_item.state,
                RuntimeState::Active { reason } if reason.contains("current working directory")
            ),
            "linked worktree should be active when CWD is a nested subdirectory, got {:?}",
            linked_item.state
        );
    }

    #[test]
    fn is_safe_maintenance_branch_rejects_protected_names_in_empty_prefix_mode() {
        // Empty-prefix mode must still reject protected names in tail components.
        assert!(
            !is_safe_maintenance_branch("epic/main", ""),
            "epic/main should be rejected"
        );
        assert!(
            !is_safe_maintenance_branch("epic/master", ""),
            "epic/master should be rejected"
        );
        assert!(
            !is_safe_maintenance_branch("initiative/HEAD", ""),
            "initiative/HEAD should be rejected"
        );
        assert!(
            !is_safe_maintenance_branch("runs/HEAD/agent-1", ""),
            "runs/HEAD/agent-1 should be rejected"
        );
        assert!(
            !is_safe_maintenance_branch("archive/develop", ""),
            "archive/develop should be rejected"
        );
        assert!(
            !is_safe_maintenance_branch("runs/trunk", ""),
            "runs/trunk should be rejected"
        );

        // Valid empty-prefix branches should still pass.
        assert!(is_safe_maintenance_branch("epic/phase-3", ""));
        assert!(is_safe_maintenance_branch("runs/session-a/agent-1", ""));
        assert!(is_safe_maintenance_branch("archive/old", ""));
    }
}

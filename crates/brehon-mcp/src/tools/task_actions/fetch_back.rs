//! Publishing an agent's commits into canonical (ADR-0010 T10/T11).
//!
//! Under per-agent clones a worker's commits exist only in its own repository.
//! Everything downstream — the reviewer panel, `resolve_review_commit_set`,
//! `cherry-pick -x`, `git_patch_id` — reads canonical, so the commits have to
//! be published there the moment the worker says it is done. This runs at
//! `complete`/`checkpoint` rather than at integration because **review** is the
//! first consumer, not integration.
//!
//! Direction is load-bearing: Brehon fetches *from* the agent repository and
//! never pushes into one. A fetch from a hostile repository executes nothing,
//! while a push into one fires six code paths as the pushing user.

use std::path::{Path, PathBuf};

/// Above this, a fetch-back is refused and its refs are dropped.
///
/// Nothing else bounds what the fetch pulls in. One agent that commits
/// `target/` permanently inflates canonical, and under worktrees this was a
/// non-event because the objects were already in the shared store. Sized to
/// pass anything a person would call a change and fail a build directory.
const MAX_FETCH_GROWTH_KIB: u64 = 256 * 1024;
const MAX_FETCH_GROWTH_OBJECTS: u64 = 50_000;

/// Canonical — the project repository, never an agent's clone.
///
/// Deliberately not `workspace_root()`, which resolves to the *caller's* own
/// directory: at `complete` the caller is the worker, and its clone is the one
/// place these commits already exist.
fn canonical_root() -> Option<PathBuf> {
    if let Ok(root) = std::env::var("BREHON_PROJECT_ROOT") {
        let root = root.trim();
        if !root.is_empty() {
            return Some(PathBuf::from(root));
        }
    }
    let brehon_root = PathBuf::from(std::env::var("BREHON_ROOT").ok()?);
    (brehon_root.file_name()?.to_str()? == ".brehon")
        .then(|| brehon_root.parent().map(PathBuf::from))
        .flatten()
}

/// The calling agent's own repository, if it is one.
fn caller_agent_dir() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var("BREHON_WORKSPACE_ROOT").ok()?);
    root.join(".git").exists().then_some(root)
}

fn caller_agent_id() -> Option<String> {
    let name = std::env::var("BREHON_AGENT_NAME").ok()?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Publish the calling agent's `brehon/*` branches into canonical.
///
/// A no-op when the caller is not an isolated agent — the operator's own
/// terminal, or a run without worktree isolation, has nothing to publish
/// because its commits are already in canonical.
///
/// `Err` is a blocking condition the caller must surface: if the commits did
/// not reach canonical, review cannot see them and integration cannot apply
/// them, and failing silently would strand the work with a green task status.
pub(super) fn publish_agent_commits() -> Result<(), String> {
    let (Some(canonical), Some(agent_dir), Some(agent_id)) =
        (canonical_root(), caller_agent_dir(), caller_agent_id())
    else {
        return Ok(());
    };
    if same_repository(&canonical, &agent_dir) {
        return Ok(());
    }

    let growth =
        brehon_git::fetch_back_from_agent(&canonical, &agent_dir, &agent_id).map_err(|err| {
            format!(
                "Failed to publish commits from '{}' into the project repository: {err}. \
             A non-fast-forward here means the branch was rewritten after it was reviewed; \
             add a new commit instead of amending or rebasing.",
                agent_dir.display()
            )
        })?;

    if growth.kib > MAX_FETCH_GROWTH_KIB || growth.objects > MAX_FETCH_GROWTH_OBJECTS {
        let dropped = drop_agent_namespace(&canonical, &agent_id);
        return Err(format!(
            "Refusing to publish commits from '{}': the fetch added {} MiB across {} objects, \
             above the {} MiB / {} object ceiling. This is almost always a build directory or \
             dependency tree committed by mistake. Remove it from the branch and call complete \
             again.{}",
            agent_dir.display(),
            growth.kib / 1024,
            growth.objects,
            MAX_FETCH_GROWTH_KIB / 1024,
            MAX_FETCH_GROWTH_OBJECTS,
            if dropped {
                ""
            } else {
                " The fetched refs could not be dropped; run `git gc` on the project repository."
            }
        ));
    }
    Ok(())
}

/// Same directory, or the agent is not isolated at all.
fn same_repository(canonical: &Path, agent_dir: &Path) -> bool {
    match (canonical.canonicalize(), agent_dir.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => canonical == agent_dir,
    }
}

/// Delete everything the fetch just wrote under this agent's namespace, so an
/// over-sized fetch leaves its objects unreachable rather than permanently
/// inflating canonical.
fn drop_agent_namespace(canonical: &Path, agent_id: &str) -> bool {
    let prefix = format!("refs/heads/brehon/{agent_id}/");
    let Ok(output) = crate::git_exec::run_git(
        canonical,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("{prefix}**"),
        ],
    ) else {
        return false;
    };
    let refs: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();

    refs.iter().all(|reference| {
        crate::git_exec::run_git(canonical, &["update-ref", "-d", reference])
            .is_ok_and(|out| out.status.success())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_repository_ignores_path_spelling() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let indirect = temp.path().join("repo").join(".").join(".");

        assert!(same_repository(&root, &indirect));
        assert!(!same_repository(&root, temp.path()));
    }

    /// The growth ceiling has to reject a build directory and accept a change.
    #[test]
    fn growth_ceiling_separates_a_change_from_a_build_tree() {
        let ordinary = brehon_git::FetchBackGrowth {
            objects: 40,
            kib: 900,
        };
        let build_tree = brehon_git::FetchBackGrowth {
            objects: 120_000,
            kib: 700 * 1024,
        };

        assert!(
            ordinary.kib <= MAX_FETCH_GROWTH_KIB && ordinary.objects <= MAX_FETCH_GROWTH_OBJECTS
        );
        assert!(
            build_tree.kib > MAX_FETCH_GROWTH_KIB || build_tree.objects > MAX_FETCH_GROWTH_OBJECTS
        );
    }
}

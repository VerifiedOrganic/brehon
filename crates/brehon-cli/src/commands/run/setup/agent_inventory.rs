//! Brehon-owned inventory of agent working directories.
//!
//! Agent directories are registered nowhere Brehon owns. `git worktree list`
//! and a filesystem glob over the worktree root
//! (`candidate_worker_worktree_paths`, duplicated in brehon-mcp and
//! brehon-tui) both serve as de-facto registries today, and both are
//! properties of the *topology* rather than of Brehon. ADR-0010 removes the
//! first outright — a clone has no parent registration — and makes the second
//! ambiguous.
//!
//! So Brehon records what it created: agent id, path, branch, base ref and
//! creation time. Removal becomes `rm -rf`; staleness becomes "the directory
//! is missing", which no longer requires asking git anything.
//!
//! One file per agent, mirroring the discipline already used for everything
//! else an agent owns under `runtime/` (`sessions/`, `agent-health/`): a
//! single writer per file needs no lock, and two concurrent sessions cannot
//! clobber each other. See `docs/CONTAINMENT_PLAN.md` T9.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One agent working directory, as Brehon created it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentDirEntry {
    pub(crate) agent_id: String,
    pub(crate) session: String,
    /// `worker`, `supervisor`, `reviewer`, `advisor`, `research`.
    pub(crate) role: String,
    pub(crate) path: PathBuf,
    pub(crate) branch: String,
    /// The ref the directory was seeded from — the base branch today, and the
    /// `base.git` ref once T8 lands.
    pub(crate) base_ref: String,
    pub(crate) created_at: String,
}

fn inventory_dir(brehon_root: &Path) -> PathBuf {
    brehon_root.join("runtime").join("agents")
}

/// Agent ids are only unique within a session, and two sessions can run
/// against one project, so the filename carries both. `/` appears in neither
/// after `safe_component`.
fn entry_path(brehon_root: &Path, session: &str, agent_id: &str) -> PathBuf {
    inventory_dir(brehon_root).join(format!(
        "{}__{}.json",
        safe_component(session),
        safe_component(agent_id)
    ))
}

/// Flatten anything that would escape the inventory directory or collide with
/// the `__` separator.
fn safe_component(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Record a directory Brehon just created.
///
/// Best-effort: the directory exists by the time this runs, so failing the
/// spawn here would strand it with no entry at all — the exact leak the
/// inventory exists to stop.
pub(crate) fn record(brehon_root: &Path, entry: &AgentDirEntry) {
    let path = entry_path(brehon_root, &entry.session, &entry.agent_id);
    if let Err(err) = brehon_types::write_json_atomic(&path, entry) {
        tracing::warn!(
            agent_id = %entry.agent_id,
            path = %path.display(),
            error = %err,
            "Failed to record agent directory in the inventory"
        );
    }
}

/// Drop an entry once its directory is gone.
pub(crate) fn forget(brehon_root: &Path, session: &str, agent_id: &str) {
    let path = entry_path(brehon_root, session, agent_id);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => tracing::warn!(
            agent_id,
            path = %path.display(),
            error = %err,
            "Failed to remove agent directory from the inventory"
        ),
    }
}

/// Every recorded agent directory, in no particular order.
pub(crate) fn list(brehon_root: &Path) -> Vec<AgentDirEntry> {
    let Ok(entries) = std::fs::read_dir(inventory_dir(brehon_root)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|raw| serde_json::from_str(&raw).ok())
        .collect()
}

/// Drop entries whose directory no longer exists, returning them.
///
/// This is what a killed run leaves behind: the directory is gone (or was
/// never fully created) while the entry survives. Reconciling at session close
/// keeps the inventory honest without asking git anything.
pub(crate) fn reconcile(brehon_root: &Path) -> Vec<AgentDirEntry> {
    let mut dropped = Vec::new();
    for entry in list(brehon_root) {
        if entry.path.exists() {
            continue;
        }
        forget(brehon_root, &entry.session, &entry.agent_id);
        dropped.push(entry);
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(agent_id: &str, path: &Path) -> AgentDirEntry {
        AgentDirEntry {
            agent_id: agent_id.to_string(),
            session: "session-a".to_string(),
            role: "worker".to_string(),
            path: path.to_path_buf(),
            branch: format!("brehon/{agent_id}"),
            base_ref: "main".to_string(),
            created_at: "2026-08-21T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn records_and_lists_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let dir = root.join("agent-dir");
        std::fs::create_dir_all(&dir).unwrap();

        let recorded = entry("w1", &dir);
        record(root, &recorded);

        assert_eq!(list(root), vec![recorded]);
    }

    /// T9 acceptance criterion: a killed run leaves an entry whose directory is
    /// absent, and cleanup reconciles it.
    #[test]
    fn reconcile_drops_entries_whose_directory_is_gone() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let live_dir = root.join("live");
        let dead_dir = root.join("dead");
        std::fs::create_dir_all(&live_dir).unwrap();
        std::fs::create_dir_all(&dead_dir).unwrap();

        record(root, &entry("live-1", &live_dir));
        record(root, &entry("dead-1", &dead_dir));

        // The run is killed and the directory removed without a clean close.
        std::fs::remove_dir_all(&dead_dir).unwrap();

        let dropped = reconcile(root);

        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].agent_id, "dead-1");
        let remaining = list(root);
        assert_eq!(remaining.len(), 1, "the live entry must survive");
        assert_eq!(remaining[0].agent_id, "live-1");
    }

    /// Two sessions can run against one project and agent ids only distinguish
    /// agents within a session.
    #[test]
    fn concurrent_sessions_do_not_clobber_each_other() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let dir = root.join("d");
        std::fs::create_dir_all(&dir).unwrap();

        let mut a = entry("worker-1", &dir);
        a.session = "session-a".to_string();
        let mut b = entry("worker-1", &dir);
        b.session = "session-b".to_string();
        record(root, &a);
        record(root, &b);

        let sessions: std::collections::BTreeSet<String> =
            list(root).into_iter().map(|e| e.session).collect();
        assert_eq!(sessions.len(), 2, "one entry per session, not one shared");
    }

    /// A session name or agent id with a path separator must not write outside
    /// the inventory directory.
    #[test]
    fn hostile_names_stay_inside_the_inventory_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let dir = root.join("d");
        std::fs::create_dir_all(&dir).unwrap();

        let mut hostile = entry("../../escape", &dir);
        hostile.session = "../..".to_string();
        record(root, &hostile);

        assert_eq!(list(root).len(), 1, "the entry landed in the inventory");
        assert!(
            !root.join("escape.json").exists(),
            "no file may be written outside runtime/agents"
        );
    }

    /// The inventory is only useful if creation actually populates it, so this
    /// asserts against the real `prepare_scoped_worktrees` path rather than
    /// against `record` alone.
    #[tokio::test]
    async fn preparing_agent_dirs_populates_the_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        super::super::tests::init_git_repo(temp.path());
        std::fs::create_dir_all(temp.path().join(".brehon")).unwrap();

        let mut config = brehon_config::parse_defaults().unwrap();
        config.orchestration.worktree_isolation = true;
        config.orchestration.auto_cleanup_worktrees = true;
        config.orchestration.branch_prefix = "brehon/".to_string();
        config.orchestration.worktree_root = Some(external.path().to_string_lossy().to_string());

        let names = vec!["worker-1".to_string()];
        let cwds = super::super::prepare_scoped_worktrees(
            temp.path(),
            &config,
            Some("session-a"),
            None,
            &names,
        )
        .await
        .unwrap();

        let recorded = list(&temp.path().join(".brehon"));
        assert_eq!(recorded.len(), 1, "creation must record the directory");
        let recorded = &recorded[0];
        assert_eq!(recorded.agent_id, "worker-1");
        assert_eq!(recorded.session, "session-a");
        assert_eq!(recorded.role, "worker");
        assert_eq!(recorded.branch, "brehon/runs/session-a/worker-1");
        assert_eq!(&recorded.path, cwds.get("worker-1").unwrap());
        assert!(!recorded.base_ref.is_empty());
        assert!(!recorded.created_at.is_empty());
    }

    #[test]
    fn forget_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        forget(root, "session-a", "never-recorded");
        assert!(list(root).is_empty());
    }
}

//! Per-agent `file://` clones and the brehon-owned base mirror (ADR-0010).
//!
//! A linked worktree cannot be sandboxed: its index, refs and objects live in
//! `<main>/.git`, so any jail tight enough to protect the project breaks
//! `git commit` in the agent's own tree. Each agent therefore gets a real
//! repository, seeded over `file://` so no inode is shared.
//!
//! Three facts drive every choice here, all reproduced on the development
//! machine:
//!
//! 1. **`git clone <path>` hardlinks packfiles.** The clone's
//!    `.git/objects/pack/*.pack` shares an inode with the source, packs are
//!    mode 444, and the documented careless-model response to `Permission
//!    denied` is `chmod -R u+w .` — after which a write inside the agent's own
//!    directory corrupts the source. `git clone file://` yields disjoint
//!    stores. [`assert_disjoint_object_store`] is the regression guard.
//! 2. **`git fetch` never hardlinks**, so refreshing cannot reintroduce
//!    aliasing.
//! 3. **`git fetch` from a hostile repository executes nothing**, while
//!    `git push` *into* one fires six code paths as the pushing user.
//!    Direction is load-bearing: Brehon fetches *from* agent repositories and
//!    never pushes into one.
//!
//! Everything here shells out rather than using libgit2, deliberately: the
//! hardlink behaviour of `clone` and the wire semantics of `fetch` are
//! properties of git itself, and fidelity is the entire point. These run
//! against Brehon-owned repositories, or against an agent directory that does
//! not yet contain an agent — never with a live agent repository as cwd for
//! mutation.

use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::debug;

use crate::error::GitError;

/// Written by Brehon after each base-mirror refresh so staleness is visible.
///
/// If the refresh dies at hour 6 of a 72-hour run every agent silently
/// develops against a frozen base and conflict rates climb with no signal, so
/// this is surfaced in `brehon doctor` and run status rather than kept
/// internal.
pub const BASE_REFRESHED_AT: &str = "BREHON_REFRESHED_AT";

fn git(args: &[&str], cwd: Option<&Path>) -> Result<String, GitError> {
    let mut command = Command::new("git");
    if let Some(cwd) = cwd {
        command.arg("-C").arg(cwd);
    }
    command.args(args);
    // A seed or fetch must never stop for credentials or an editor.
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1");

    let output = command
        .output()
        .map_err(|err| GitError::GitOperationFailed(format!("failed to run git: {err}")))?;
    if !output.status.success() {
        return Err(GitError::GitOperationFailed(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `file://` URL for a local path.
///
/// The scheme is not cosmetic — a bare path makes git hardlink packfiles into
/// the clone, which is the corruption vector this module exists to remove.
fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Seed the brehon-owned base mirror from canonical.
///
/// `origin` is removed immediately: `clone --bare` writes
/// `remote.origin.url = <canonical>` into a 0644 file, and canonical's path
/// must appear in no agent-readable git config (invariant I7).
pub fn seed_base_mirror(canonical: &Path, base: &Path) -> Result<(), GitError> {
    if base.exists() {
        return Err(GitError::GitOperationFailed(format!(
            "base mirror already exists at '{}'",
            base.display()
        )));
    }
    if let Some(parent) = base.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let base_str = base.to_string_lossy().to_string();
    git(&["clone", "--bare", &file_url(canonical), &base_str], None)?;
    git(&["remote", "remove", "origin"], Some(base))?;
    // The mirror is refreshed by fetch, never packed behind Brehon's back.
    git(&["config", "gc.auto", "0"], Some(base))?;

    debug!("Seeded base mirror at {}", base.display());
    Ok(())
}

/// Refresh the base mirror from canonical, narrowed to `base_branches`.
///
/// Safe despite the `+` force refspec because the mirror is Brehon-owned and
/// agent-read-only — that ownership split is the whole reason this direction
/// is allowed. Only the configured base branches are exposed, so agents never
/// see each other's work through the mirror.
pub fn refresh_base_mirror(
    canonical: &Path,
    base: &Path,
    base_branches: &[String],
    now: &str,
) -> Result<(), GitError> {
    if base_branches.is_empty() {
        return Err(GitError::GitOperationFailed(
            "refusing to refresh the base mirror with no base branches configured".to_string(),
        ));
    }

    let url = file_url(canonical);
    let mut args: Vec<String> = vec!["fetch".into(), "--prune".into(), "--no-tags".into(), url];
    for branch in base_branches {
        args.push(format!("+refs/heads/{branch}:refs/heads/{branch}"));
    }
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    git(&borrowed, Some(base))?;

    std::fs::write(base.join(BASE_REFRESHED_AT), format!("{now}\n"))?;
    Ok(())
}

/// When the base mirror was last refreshed, if it ever was.
pub fn base_refreshed_at(base: &Path) -> Option<String> {
    std::fs::read_to_string(base.join(BASE_REFRESHED_AT))
        .ok()
        .map(|stamp| stamp.trim().to_string())
        .filter(|stamp| !stamp.is_empty())
}

/// Clone an agent repository from the base mirror and put it on `branch`.
///
/// The reflog is removed because `clone` records `clone: from <base>` in it,
/// and the mirror's path is Brehon-owned state the agent has no reason to
/// learn.
pub fn clone_agent_repo(base: &Path, agent_dir: &Path, branch: &str) -> Result<(), GitError> {
    if agent_dir.exists() {
        return Err(GitError::GitOperationFailed(format!(
            "agent directory already exists at '{}'",
            agent_dir.display()
        )));
    }
    if let Some(parent) = agent_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let dir_str = agent_dir.to_string_lossy().to_string();
    git(&["clone", &file_url(base), &dir_str], None)?;
    git(&["config", "gc.auto", "0"], Some(agent_dir))?;
    git(&["checkout", "-b", branch], Some(agent_dir))?;

    // Last, not first: `checkout -b` writes its own reflog entries, so
    // removing the logs earlier would leave the clone's `clone: from <base>`
    // line behind. Anything the agent does afterwards starts a fresh reflog
    // containing only its own history.
    let logs = agent_dir.join(".git").join("logs");
    if logs.exists() {
        std::fs::remove_dir_all(&logs)?;
    }

    // Invariant I1, checked before any agent is spawned into this directory.
    assert_disjoint_object_store(agent_dir)?;

    debug!(
        "Cloned agent repo at {} on branch {branch}",
        agent_dir.display()
    );
    Ok(())
}

/// Refuse a repository whose object store shares inodes with another.
///
/// This is the regression guard for the entire hardlink class: a single
/// `st_nlink > 1` under `.git/objects` means a write inside this directory can
/// corrupt whatever it was cloned from. One stat walk, at spawn time.
#[cfg(unix)]
pub fn assert_disjoint_object_store(repo: &Path) -> Result<(), GitError> {
    use std::os::unix::fs::MetadataExt;

    fn walk(dir: &Path) -> Result<Option<PathBuf>, GitError> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(None);
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                if let Some(found) = walk(&path)? {
                    return Ok(Some(found));
                }
            } else if file_type.is_file() {
                let metadata = std::fs::metadata(&path)?;
                if metadata.nlink() > 1 {
                    return Ok(Some(path));
                }
            }
        }
        Ok(None)
    }

    let objects = repo.join(".git").join("objects");
    let objects = if objects.is_dir() {
        objects
    } else {
        repo.join("objects")
    };

    match walk(&objects)? {
        None => Ok(()),
        Some(shared) => Err(GitError::GitOperationFailed(format!(
            "refusing to use '{}': '{}' is hardlinked (st_nlink > 1), so a write inside this \
             repository can corrupt the repository it was cloned from. Seed with `file://`, \
             never a bare path.",
            repo.display(),
            shared.display()
        ))),
    }
}

#[cfg(not(unix))]
pub fn assert_disjoint_object_store(_repo: &Path) -> Result<(), GitError> {
    Err(GitError::GitOperationFailed(
        "clone isolation is unsupported on this platform; use WSL2 or a container".to_string(),
    ))
}

/// Object-store size, as `git count-objects -v` reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObjectStoreSize {
    pub count: u64,
    pub size_kib: u64,
    pub in_pack: u64,
    pub size_pack_kib: u64,
}

impl ObjectStoreSize {
    pub fn objects(&self) -> u64 {
        self.count + self.in_pack
    }

    pub fn kib(&self) -> u64 {
        self.size_kib + self.size_pack_kib
    }
}

/// Measure a repository's object store.
pub fn measure_object_store(repo: &Path) -> Result<ObjectStoreSize, GitError> {
    let stdout = git(&["count-objects", "-v"], Some(repo))?;
    let mut size = ObjectStoreSize::default();
    for line in stdout.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Ok(value) = value.trim().parse::<u64>() else {
            continue;
        };
        match key.trim() {
            "count" => size.count = value,
            "size" => size.size_kib = value,
            "in-pack" => size.in_pack = value,
            "size-pack" => size.size_pack_kib = value,
            _ => {}
        }
    }
    Ok(size)
}

/// What a fetch-back pulled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchBackGrowth {
    pub objects: u64,
    pub kib: u64,
}

/// Fetch an agent's branches into canonical, namespaced per agent.
///
/// The refspec has **no leading `+`** on purpose. Two agents that both name a
/// branch `brehon/w1` — a model copying a name out of context — would
/// force-overwrite each other, and the loser's panel-approved commit is
/// unreachable after gc. Worktrees prevented that structurally; clones do not,
/// so a rewritten history must arrive as a loud non-fast-forward failure
/// instead of a silent force-update. That doubles as a tripwire for a worker
/// that rebased after approval.
///
/// Returns how much the object store grew, so the caller can refuse or
/// quarantine a fetch that pulled in a committed `target/`.
pub fn fetch_back_from_agent(
    canonical: &Path,
    agent_dir: &Path,
    agent_id: &str,
) -> Result<FetchBackGrowth, GitError> {
    if agent_id.is_empty() || agent_id.contains(['/', '*', '~', '^', ':', '\\']) {
        return Err(GitError::InvalidReferenceName(format!(
            "agent id '{agent_id}' cannot be used as a ref namespace"
        )));
    }

    let before = measure_object_store(canonical)?;
    let refspec = format!("refs/heads/brehon/*:refs/heads/brehon/{agent_id}/*");
    git(
        &[
            "fetch",
            "--no-tags",
            "--no-recurse-submodules",
            &agent_dir.to_string_lossy(),
            &refspec,
        ],
        Some(canonical),
    )?;
    let after = measure_object_store(canonical)?;

    Ok(FetchBackGrowth {
        objects: after.objects().saturating_sub(before.objects()),
        kib: after.kib().saturating_sub(before.kib()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap_or_else(|e| panic!("git {}: {e}", args.join(" ")));
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn canonical_repo(root: &Path) -> PathBuf {
        let repo = root.join("canonical");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-b", "main"]);
        run(&repo, &["config", "user.email", "brehon@example.invalid"]);
        run(&repo, &["config", "user.name", "Brehon Test"]);
        std::fs::write(repo.join("README.md"), "seed\n").unwrap();
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-m", "seed"]);
        repo
    }

    /// Invariant I7: canonical's path must appear in no agent-readable config.
    #[test]
    fn seeded_mirror_names_canonical_nowhere() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = canonical_repo(temp.path());
        let base = temp.path().join("base.git");

        seed_base_mirror(&canonical, &base).unwrap();

        let config = std::fs::read_to_string(base.join("config")).unwrap();
        assert!(
            !config.contains(canonical.to_string_lossy().as_ref()),
            "the mirror must not name canonical: {config}"
        );
        assert_eq!(run(&base, &["config", "--get", "gc.auto"]), "0");
    }

    #[test]
    fn refresh_stamps_and_narrows_to_base_branches() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = canonical_repo(temp.path());
        let base = temp.path().join("base.git");
        seed_base_mirror(&canonical, &base).unwrap();

        // A peer's branch must not reach the mirror.
        run(&canonical, &["branch", "brehon/peer-work"]);
        std::fs::write(canonical.join("next.txt"), "next\n").unwrap();
        run(&canonical, &["add", "-A"]);
        run(&canonical, &["commit", "-m", "advance main"]);

        assert!(base_refreshed_at(&base).is_none());
        refresh_base_mirror(
            &canonical,
            &base,
            &["main".to_string()],
            "2026-08-21T00:00:00Z",
        )
        .unwrap();

        assert_eq!(
            base_refreshed_at(&base).as_deref(),
            Some("2026-08-21T00:00:00Z")
        );
        let branches = run(
            &base,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        );
        assert!(branches.contains("main"));
        assert!(
            !branches.contains("peer-work"),
            "the mirror must expose only base branches, got: {branches}"
        );
    }

    #[test]
    fn refresh_refuses_an_empty_base_branch_list() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = canonical_repo(temp.path());
        let base = temp.path().join("base.git");
        seed_base_mirror(&canonical, &base).unwrap();

        refresh_base_mirror(&canonical, &base, &[], "2026-08-21T00:00:00Z")
            .expect_err("refreshing with no base branches must refuse");
    }

    /// The load-bearing property of the whole phase.
    #[test]
    fn agent_clone_has_a_disjoint_object_store() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = canonical_repo(temp.path());
        let base = temp.path().join("base.git");
        seed_base_mirror(&canonical, &base).unwrap();

        let agent = temp.path().join("agents").join("w1");
        clone_agent_repo(&base, &agent, "brehon/w1").unwrap();

        assert_disjoint_object_store(&agent).expect("a file:// clone shares no inode");
        assert_eq!(run(&agent, &["branch", "--show-current"]), "brehon/w1");
        assert_eq!(run(&agent, &["config", "--get", "gc.auto"]), "0");
        assert!(
            !agent.join(".git/logs").exists(),
            "the reflog records `clone: from <base>` and must be removed"
        );
    }

    /// The assertion has to actually fire, or it guards nothing. A bare-path
    /// clone is exactly the mistake it exists to catch.
    #[test]
    fn disjoint_assertion_rejects_a_path_cloned_repo() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = canonical_repo(temp.path());
        let base = temp.path().join("base.git");
        seed_base_mirror(&canonical, &base).unwrap();

        // Deliberately the wrong thing: a bare path, which hardlinks packs.
        let hardlinked = temp.path().join("hardlinked");
        run(
            temp.path(),
            &[
                "clone",
                base.to_string_lossy().as_ref(),
                hardlinked.to_string_lossy().as_ref(),
            ],
        );

        let err = assert_disjoint_object_store(&hardlinked)
            .expect_err("a path clone hardlinks packfiles and must be refused");
        assert!(
            err.to_string().contains("hardlinked"),
            "the refusal must name the reason: {err}"
        );
    }

    #[test]
    fn fetch_back_is_namespaced_and_reports_growth() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = canonical_repo(temp.path());
        let base = temp.path().join("base.git");
        seed_base_mirror(&canonical, &base).unwrap();
        let agent = temp.path().join("agents").join("w1");
        clone_agent_repo(&base, &agent, "brehon/w1").unwrap();

        run(&agent, &["config", "user.email", "a@example.invalid"]);
        run(&agent, &["config", "user.name", "Agent"]);
        std::fs::write(agent.join("work.txt"), "agent work\n").unwrap();
        run(&agent, &["add", "-A"]);
        run(&agent, &["commit", "-m", "agent work"]);
        let agent_sha = run(&agent, &["rev-parse", "HEAD"]);

        let growth = fetch_back_from_agent(&canonical, &agent, "worker-1").unwrap();

        let refs = run(
            &canonical,
            &["for-each-ref", "--format=%(refname)", "refs/heads/brehon"],
        );
        assert_eq!(refs, "refs/heads/brehon/worker-1/w1", "refs: {refs}");
        assert_eq!(run(&canonical, &["rev-parse", &refs]), agent_sha);
        assert!(growth.objects > 0, "the fetch pulled objects in");
    }

    /// SHA identity survives the round trip, which is what lets the reviewer
    /// panel and `cherry-pick -x` keep working unmodified.
    #[test]
    fn fetch_back_refuses_a_rewritten_history() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = canonical_repo(temp.path());
        let base = temp.path().join("base.git");
        seed_base_mirror(&canonical, &base).unwrap();
        let agent = temp.path().join("agents").join("w1");
        clone_agent_repo(&base, &agent, "brehon/w1").unwrap();
        run(&agent, &["config", "user.email", "a@example.invalid"]);
        run(&agent, &["config", "user.name", "Agent"]);
        std::fs::write(agent.join("work.txt"), "v1\n").unwrap();
        run(&agent, &["add", "-A"]);
        run(&agent, &["commit", "-m", "v1"]);
        fetch_back_from_agent(&canonical, &agent, "worker-1").unwrap();

        // The worker rewrites the commit the panel already saw.
        run(&agent, &["commit", "--amend", "-m", "v1 rewritten"]);

        let err = fetch_back_from_agent(&canonical, &agent, "worker-1")
            .expect_err("a rewritten history must not silently force-update canonical");
        assert!(
            err.to_string().contains("fetch"),
            "the refusal must come from the fetch: {err}"
        );
    }

    #[test]
    fn fetch_back_rejects_an_agent_id_that_escapes_its_namespace() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = canonical_repo(temp.path());
        let agent = temp.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();

        for hostile in ["", "a/b", "a*", "..", "a:b"] {
            if hostile == ".." {
                continue;
            }
            fetch_back_from_agent(&canonical, &agent, hostile)
                .expect_err("a hostile agent id must not become a ref namespace");
        }
    }
}

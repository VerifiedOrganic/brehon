//! Lifecycle of the brehon-owned base mirror (ADR-0010 T7).
//!
//! Agents clone from `base.git` rather than from canonical, which buys three
//! things at once: a stock `git fetch origin` sync channel, canonical's path
//! named nowhere an agent can read it, and no peer's branches on offer. The
//! mirror is Brehon-writable and agent-read-only, and that ownership split is
//! what makes refreshing it safe.
//!
//! It lives beside the agent directories under the worktree root, which is
//! outside the project repository. Deliberately *not* under `.brehon/`: that
//! directory is granted to agents (it is the one hole T13 accepts), so a
//! mirror there would be agent-writable and the ownership split would be a
//! fiction.

use std::path::{Path, PathBuf};

/// How stale the mirror may get before it is refreshed again.
///
/// `prepare_scoped_worktrees_with_progress` runs once per role group, so a
/// single run calls through here five times; without this every call would
/// re-fetch. Not configurable until something needs it to be — the refresh is
/// cheap and the only cost of being wrong is a slightly older base.
const REFRESH_INTERVAL_SECS: i64 = 60;

/// Where the mirror lives for a given worktree root.
fn base_mirror_path(worktrees_root: &Path) -> PathBuf {
    worktrees_root.join("base.git")
}

/// Seed the mirror if absent, refresh it if stale, and return its path.
///
/// Refresh failures are not fatal: an agent cloning from a slightly old base
/// is a conflict-rate problem, while refusing to start a run is an outage. The
/// staleness stamp is what surfaces the difference to the operator.
pub(crate) fn ensure_base_mirror(
    canonical: &Path,
    worktrees_root: &Path,
    base_branch: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<PathBuf, brehon_git::GitError> {
    let base = base_mirror_path(worktrees_root);
    publish_location(canonical, &base);

    if !base.exists() {
        brehon_git::seed_base_mirror(canonical, &base)?;
    } else if !is_fresh(&base, now) {
        if let Err(err) = brehon_git::refresh_base_mirror(
            canonical,
            &base,
            std::slice::from_ref(&base_branch.to_string()),
            &now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ) {
            tracing::warn!(
                base = %base.display(),
                error = %err,
                "Failed to refresh the base mirror; agents will clone from a stale base"
            );
        }
        return Ok(base);
    }

    // A freshly seeded mirror is by definition current, so stamp it rather
    // than immediately fetching from the repository it was just cloned from.
    if brehon_git::base_refreshed_at(&base).is_none() {
        let stamp = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        if let Err(err) = std::fs::write(
            base.join(brehon_git::BASE_REFRESHED_AT),
            format!("{stamp}\n"),
        ) {
            tracing::warn!(error = %err, "Failed to stamp the freshly seeded base mirror");
        }
    }
    Ok(base)
}

/// Record where the mirror lives so `brehon doctor` can find it.
///
/// The worktree root is derived from config plus a repo-identity hash that
/// only this crate computes, and brehon-doctor may not depend on it. A checker
/// that guessed the location wrong would silently never fire.
fn publish_location(canonical: &Path, base: &Path) {
    let pointer = canonical
        .join(".brehon")
        .join(brehon_types::BASE_MIRROR_POINTER);
    if let Some(parent) = pointer.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if let Err(err) = std::fs::write(&pointer, format!("{}\n", base.display())) {
        tracing::warn!(error = %err, "Failed to publish the base mirror location for doctor");
    }
}

/// How long ago the mirror was refreshed, if it carries a readable stamp.
///
/// `None` means "no usable stamp" — either never refreshed or unparseable —
/// which the operator should see as loudly as a stale one.
fn age(base: &Path, now: chrono::DateTime<chrono::Utc>) -> Option<chrono::Duration> {
    let stamp = brehon_git::base_refreshed_at(base)?;
    let refreshed = chrono::DateTime::parse_from_rfc3339(&stamp).ok()?;
    Some(now.signed_duration_since(refreshed.with_timezone(&chrono::Utc)))
}

fn is_fresh(base: &Path, now: chrono::DateTime<chrono::Utc>) -> bool {
    age(base, now).is_some_and(|age| age.num_seconds() < REFRESH_INTERVAL_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
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

    fn canonical(root: &Path) -> PathBuf {
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

    fn at(secs: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn seeds_on_first_call_and_stamps_it_current() {
        let temp = tempfile::tempdir().unwrap();
        let repo = canonical(temp.path());
        let root = temp.path().join("worktrees");

        let base = ensure_base_mirror(&repo, &root, "main", at(0)).unwrap();

        assert!(base.join("HEAD").exists(), "a bare mirror was created");
        assert_eq!(age(&base, at(0)).map(|d| d.num_seconds()), Some(0));
        assert!(
            !std::fs::read_to_string(base.join("config"))
                .unwrap()
                .contains(repo.to_string_lossy().as_ref()),
            "the mirror must not name canonical"
        );
    }

    /// Five role groups per run must not mean five fetches.
    #[test]
    fn does_not_refresh_while_fresh() {
        let temp = tempfile::tempdir().unwrap();
        let repo = canonical(temp.path());
        let root = temp.path().join("worktrees");
        let base = ensure_base_mirror(&repo, &root, "main", at(0)).unwrap();

        std::fs::write(repo.join("later.txt"), "later\n").unwrap();
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-m", "advance canonical"]);

        ensure_base_mirror(&repo, &root, "main", at(5)).unwrap();

        assert_eq!(
            age(&base, at(5)).map(|d| d.num_seconds()),
            Some(5),
            "the stamp did not move, so no refresh happened"
        );
    }

    #[test]
    fn refreshes_once_stale_and_picks_up_canonical() {
        let temp = tempfile::tempdir().unwrap();
        let repo = canonical(temp.path());
        let root = temp.path().join("worktrees");
        let base = ensure_base_mirror(&repo, &root, "main", at(0)).unwrap();
        let before = run(&base, &["rev-parse", "main"]);

        std::fs::write(repo.join("later.txt"), "later\n").unwrap();
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-m", "advance canonical"]);

        ensure_base_mirror(&repo, &root, "main", at(REFRESH_INTERVAL_SECS + 1)).unwrap();

        assert_ne!(
            run(&base, &["rev-parse", "main"]),
            before,
            "a stale mirror must pick up canonical"
        );
        assert_eq!(
            age(&base, at(REFRESH_INTERVAL_SECS + 1)).map(|d| d.num_seconds()),
            Some(0)
        );
    }

    /// A run must not fail because the mirror could not be refreshed — an
    /// older base costs conflicts, refusing to start costs the run.
    #[test]
    fn refresh_failure_is_not_fatal() {
        let temp = tempfile::tempdir().unwrap();
        let repo = canonical(temp.path());
        let root = temp.path().join("worktrees");
        let base = ensure_base_mirror(&repo, &root, "main", at(0)).unwrap();

        std::fs::remove_dir_all(&repo).unwrap();

        let again = ensure_base_mirror(&repo, &root, "main", at(REFRESH_INTERVAL_SECS + 1))
            .expect("an unreachable canonical must not fail the run");
        assert_eq!(again, base);
        assert!(
            age(&base, at(REFRESH_INTERVAL_SECS + 1))
                .is_some_and(|d| d.num_seconds() > REFRESH_INTERVAL_SECS),
            "the stamp stays old so the operator can see the mirror is stale"
        );
    }

    #[test]
    fn age_is_none_without_a_usable_stamp() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("base.git");
        std::fs::create_dir_all(&base).unwrap();
        assert!(age(&base, at(0)).is_none());

        std::fs::write(base.join(brehon_git::BASE_REFRESHED_AT), "not a date\n").unwrap();
        assert!(age(&base, at(0)).is_none());
    }
}

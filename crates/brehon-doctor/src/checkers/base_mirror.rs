//! Base-mirror staleness checker (ADR-0010 T7).
//!
//! Agents clone from `base.git`, so if the refresh dies partway through a long
//! run every agent silently develops against a frozen base and conflict rates
//! climb with no signal at all. Staleness is the one regression versus
//! pointing `origin` straight at canonical, and the mitigation is that the
//! operator sees it *before* a run rather than in a postmortem.

use super::Checker;
use crate::types::{DiagnosticCategory, DiagnosticFinding, Severity};
use std::path::{Path, PathBuf};

/// Older than this and agents are developing against a base nobody refreshed.
const STALE_AFTER_SECS: i64 = 3600;

/// Read the refresh stamp. Mirrors brehon-git's reader; brehon-doctor may not
/// depend on brehon-git, and the filename itself lives in brehon-types.
fn base_refreshed_at(base: &Path) -> Option<String> {
    std::fs::read_to_string(base.join(brehon_types::BASE_REFRESHED_AT))
        .ok()
        .map(|stamp| stamp.trim().to_string())
        .filter(|stamp| !stamp.is_empty())
}

pub struct BaseMirrorChecker {
    /// `None` until a run has created a mirror and published where it is.
    base: Option<PathBuf>,
    now: chrono::DateTime<chrono::Utc>,
}

/// Read the mirror location Brehon published, if a run ever created one.
fn published_base(brehon_root: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(brehon_root.join(brehon_types::BASE_MIRROR_POINTER)).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

impl BaseMirrorChecker {
    pub fn new(brehon_root: &Path) -> Self {
        Self {
            base: published_base(brehon_root),
            now: chrono::Utc::now(),
        }
    }

    #[cfg(test)]
    fn at(brehon_root: &Path, now: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            base: published_base(brehon_root),
            now,
        }
    }
}

impl Checker for BaseMirrorChecker {
    fn category(&self) -> DiagnosticCategory {
        DiagnosticCategory::Worktree
    }

    fn check(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        // No mirror at all is not a finding: a project that has never run
        // agents has nothing to be stale about.
        let Some(base) = self.base.as_deref().filter(|base| base.exists()) else {
            return Ok(Vec::new());
        };

        let subject = base.display().to_string();
        let Some(stamp) = base_refreshed_at(base) else {
            return Ok(vec![DiagnosticFinding::new(
                self.category(),
                Severity::Warning,
                "Base mirror has never recorded a refresh",
            )
            .with_subject(subject)
            .with_description(
                "Agents clone from base.git. Without a refresh stamp Brehon cannot tell how old \
                 the base they are working from is."
                    .to_string(),
            )
            .with_suggestion(
                "Start a run to seed the stamp, or re-create the mirror",
            )]);
        };

        let Ok(refreshed) = chrono::DateTime::parse_from_rfc3339(&stamp) else {
            return Ok(vec![DiagnosticFinding::new(
                self.category(),
                Severity::Warning,
                "Base mirror refresh stamp is unreadable",
            )
            .with_subject(subject)
            .with_description(format!(
                "BREHON_REFRESHED_AT does not parse as RFC3339: {stamp}"
            ))
            .with_suggestion(
                "Re-create the mirror; it will be re-stamped on the next run",
            )]);
        };

        let age = self
            .now
            .signed_duration_since(refreshed.with_timezone(&chrono::Utc));
        if age.num_seconds() < STALE_AFTER_SECS {
            return Ok(Vec::new());
        }

        Ok(vec![DiagnosticFinding::new(
            self.category(),
            Severity::Warning,
            format!("Base mirror is {} hours stale", age.num_seconds() / 3600),
        )
        .with_subject(subject)
        .with_description(format!(
            "Agents clone from base.git and are developing against a base last refreshed at \
             {stamp}. Conflict rates rise the longer this goes unnoticed."
        ))
        .with_suggestion(
            "Check Brehon's logs for base mirror refresh failures",
        )])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    /// Mirrors what a real run does: create the mirror under a worktree root
    /// that is nowhere near `.brehon`, then publish where it went.
    fn mirror_with_stamp(brehon_root: &Path, stamp: Option<&str>) {
        let base = brehon_root
            .join("elsewhere")
            .join("worktrees")
            .join("base.git");
        std::fs::create_dir_all(&base).unwrap();
        if let Some(stamp) = stamp {
            std::fs::write(base.join(brehon_types::BASE_REFRESHED_AT), stamp).unwrap();
        }
        let pointer = brehon_root.join(brehon_types::BASE_MIRROR_POINTER);
        std::fs::create_dir_all(pointer.parent().unwrap()).unwrap();
        std::fs::write(pointer, format!("{}\n", base.display())).unwrap();
    }

    #[test]
    fn silent_when_there_is_no_mirror() {
        let temp = tempfile::tempdir().unwrap();
        let findings = BaseMirrorChecker::at(temp.path(), at(0)).check().unwrap();
        assert!(findings.is_empty(), "a project with no mirror is not stale");
    }

    /// The checker must not guess the location: the mirror lives under the
    /// worktree root, which is derived from config and a repo-identity hash.
    #[test]
    fn finds_a_mirror_that_is_nowhere_near_brehon_root() {
        let temp = tempfile::tempdir().unwrap();
        mirror_with_stamp(temp.path(), Some("2027-01-15T12:00:00Z\n"));
        let now = chrono::DateTime::parse_from_rfc3339("2027-01-16T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let findings = BaseMirrorChecker::at(temp.path(), now).check().unwrap();

        assert_eq!(findings.len(), 1, "the published pointer was followed");
        assert!(findings[0]
            .subject
            .as_deref()
            .is_some_and(|s| s.contains("elsewhere")));
    }

    #[test]
    fn silent_while_fresh() {
        let temp = tempfile::tempdir().unwrap();
        mirror_with_stamp(temp.path(), Some("2027-01-15T12:00:00Z\n"));
        let now = chrono::DateTime::parse_from_rfc3339("2027-01-15T12:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let findings = BaseMirrorChecker::at(temp.path(), now).check().unwrap();

        assert!(findings.is_empty(), "half an hour is not stale");
    }

    #[test]
    fn reports_a_stale_mirror() {
        let temp = tempfile::tempdir().unwrap();
        mirror_with_stamp(temp.path(), Some("2027-01-15T12:00:00Z\n"));
        let now = chrono::DateTime::parse_from_rfc3339("2027-01-16T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let findings = BaseMirrorChecker::at(temp.path(), now).check().unwrap();

        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].summary.contains("24 hours stale"),
            "the operator needs the magnitude: {}",
            findings[0].summary
        );
    }

    #[test]
    fn reports_a_mirror_that_never_recorded_a_refresh() {
        let temp = tempfile::tempdir().unwrap();
        mirror_with_stamp(temp.path(), None);

        let findings = BaseMirrorChecker::at(temp.path(), at(0)).check().unwrap();

        assert_eq!(findings.len(), 1);
        assert!(findings[0].summary.contains("never recorded"));
    }

    #[test]
    fn reports_an_unreadable_stamp() {
        let temp = tempfile::tempdir().unwrap();
        mirror_with_stamp(temp.path(), Some("last tuesday\n"));

        let findings = BaseMirrorChecker::at(temp.path(), at(0)).check().unwrap();

        assert_eq!(findings.len(), 1);
        assert!(findings[0].summary.contains("unreadable"));
    }
}

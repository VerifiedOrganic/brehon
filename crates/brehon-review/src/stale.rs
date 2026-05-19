//! Stale detection for reviews.
//!
//! Tracks when main branch moves during a review, potentially invalidating
//! the review if overlapping files are changed.

use std::collections::HashSet;
use std::sync::Arc;

use brehon_ports::GitOperations;

/// Outcome of stale detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleDetection {
    /// Review is still fresh.
    Fresh,
    /// Review is stale due to overlapping file changes.
    Stale,
}

/// Detects when reviews become stale due to main branch changes.
///
/// Tracks the base commit when a review starts and checks for
/// overlapping file changes when main moves.
pub struct StaleDetector {
    git: Arc<dyn GitOperations>,
    ignore_files: HashSet<String>,
}

impl StaleDetector {
    pub fn new(git: Arc<dyn GitOperations>) -> Self {
        Self {
            git,
            ignore_files: HashSet::new(),
        }
    }

    pub fn with_ignored_files(mut self, files: Vec<String>) -> Self {
        self.ignore_files = files.into_iter().collect();
        self
    }

    /// Check if a review has become stale.
    ///
    /// Compares the files changed in the review with files changed on main
    /// since the review started. If there's overlap, the review is stale.
    pub async fn check(
        &self,
        review_branch: &str,
        base_commit: &str,
        main_branch: &str,
    ) -> Result<StaleDetection, String> {
        let review_diff = self
            .git
            .diff(review_branch, base_commit)
            .await
            .map_err(|e| e.to_string())?;

        let main_diff = self
            .git
            .diff(main_branch, base_commit)
            .await
            .map_err(|e| e.to_string())?;

        let review_files: HashSet<&str> = review_diff
            .files
            .iter()
            .filter(|f| !self.ignore_files.contains(&f.path))
            .map(|f| f.path.as_str())
            .collect();

        let main_files: HashSet<&str> = main_diff
            .files
            .iter()
            .filter(|f| !self.ignore_files.contains(&f.path))
            .map(|f| f.path.as_str())
            .collect();

        let overlap: Vec<_> = review_files.intersection(&main_files).collect();

        if overlap.is_empty() {
            Ok(StaleDetection::Fresh)
        } else {
            tracing::info!("Review stale due to overlapping files: {:?}", overlap);
            Ok(StaleDetection::Stale)
        }
    }

    /// Check if files from two lists overlap.
    pub fn files_overlap(files_a: &[String], files_b: &[String]) -> bool {
        let set_a: HashSet<&str> = files_a.iter().map(|s| s.as_str()).collect();
        let set_b: HashSet<&str> = files_b.iter().map(|s| s.as_str()).collect();
        !set_a.is_disjoint(&set_b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_overlap_detects_overlap() {
        let files_a = vec!["src/lib.rs".to_string(), "src/main.rs".to_string()];
        let files_b = vec!["src/lib.rs".to_string(), "src/other.rs".to_string()];

        assert!(StaleDetector::files_overlap(&files_a, &files_b));
    }

    #[test]
    fn files_overlap_no_overlap() {
        let files_a = vec!["src/lib.rs".to_string()];
        let files_b = vec!["src/other.rs".to_string()];

        assert!(!StaleDetector::files_overlap(&files_a, &files_b));
    }

    #[test]
    fn stale_detection_variants() {
        assert!(matches!(StaleDetection::Fresh, StaleDetection::Fresh));
        assert!(matches!(StaleDetection::Stale, StaleDetection::Stale));
    }
}

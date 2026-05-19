//! Feedback consolidation.
//!
//! Handles:
//! - Deduplicating overlapping comments
//! - Grouping similar findings by file/symbol
//! - Categorizing: blocking vs suggestions
//! - Preserving reviewer dissent

use std::collections::HashMap;

use brehon_types::{CommentSeverity, ConsolidatedFeedback, ReviewFinding, ReviewVerdict};

use crate::panel::ReviewerSubmission;

/// Consolidates feedback from multiple reviewers.
///
/// Handles deduplication, categorization, and dissent preservation.
pub struct FeedbackConsolidator {
    similarity_threshold: f64,
}

impl Default for FeedbackConsolidator {
    fn default() -> Self {
        Self {
            similarity_threshold: 0.7,
        }
    }
}

impl FeedbackConsolidator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_similarity_threshold(threshold: f64) -> Self {
        Self {
            similarity_threshold: threshold,
        }
    }

    /// Consolidate feedback from multiple reviewer submissions.
    pub fn consolidate(&self, submissions: &[ReviewerSubmission]) -> ConsolidatedFeedback {
        let mut blocking: Vec<ReviewFinding> = Vec::new();
        let mut suggestions: Vec<ReviewFinding> = Vec::new();
        let mut nitpicks: Vec<ReviewFinding> = Vec::new();
        let mut dissent: Vec<String> = Vec::new();

        let mut seen_blockers: HashMap<String, Vec<&ReviewFinding>> = HashMap::new();
        let mut seen_suggestions: HashMap<String, Vec<&ReviewFinding>> = HashMap::new();

        for submission in submissions {
            for finding in &submission.findings {
                match finding.severity {
                    CommentSeverity::Blocking => {
                        self.add_finding_to_group(finding, &mut seen_blockers, &mut blocking);
                    }
                    CommentSeverity::Suggestion => {
                        self.add_finding_to_group(finding, &mut seen_suggestions, &mut suggestions);
                    }
                    CommentSeverity::Nitpick => {
                        nitpicks.push(finding.clone());
                    }
                }
            }
        }

        self.detect_dissent(submissions, &mut dissent);

        ConsolidatedFeedback {
            blocking,
            suggestions,
            nitpicks,
            dissent,
        }
    }

    fn add_finding_to_group<'a>(
        &self,
        finding: &'a ReviewFinding,
        seen: &mut HashMap<String, Vec<&'a ReviewFinding>>,
        output: &mut Vec<ReviewFinding>,
    ) {
        let key = self.get_grouping_key(finding);

        if let Some(existing) = seen.get(&key) {
            let is_duplicate = existing.iter().any(|f| self.is_similar(finding, f));

            if !is_duplicate {
                output.push(finding.clone());
                seen.get_mut(&key).unwrap().push(finding);
            }
        } else {
            output.push(finding.clone());
            seen.insert(key.clone(), vec![finding]);
        }
    }

    fn get_grouping_key(&self, finding: &ReviewFinding) -> String {
        if let Some(ref location) = finding.location {
            format!("{}:{}", location.file, location.line)
        } else {
            let words: Vec<&str> = finding.description.split_whitespace().take(5).collect();
            words.join(" ").to_lowercase()
        }
    }

    fn is_similar(&self, a: &ReviewFinding, b: &ReviewFinding) -> bool {
        if let (Some(loc_a), Some(loc_b)) = (&a.location, &b.location) {
            if loc_a.file == loc_b.file && (loc_a.line as i32 - loc_b.line as i32).abs() <= 3 {
                let similarity = self.text_similarity(&a.description, &b.description);
                return similarity >= self.similarity_threshold;
            }
        }

        let similarity = self.text_similarity(&a.description, &b.description);
        similarity >= self.similarity_threshold
    }

    fn text_similarity(&self, a: &str, b: &str) -> f64 {
        let words_a: std::collections::HashSet<String> = a
            .to_lowercase()
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();
        let words_b: std::collections::HashSet<String> = b
            .to_lowercase()
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();

        if words_a.is_empty() || words_b.is_empty() {
            return 0.0;
        }

        let intersection = words_a.intersection(&words_b).count();
        let union = words_a.union(&words_b).count();

        if union == 0 {
            return 0.0;
        }

        intersection as f64 / union as f64
    }

    fn detect_dissent(&self, submissions: &[ReviewerSubmission], dissent: &mut Vec<String>) {
        let mut verdicts_by_file: HashMap<String, Vec<(String, ReviewVerdict)>> = HashMap::new();

        for submission in submissions {
            for finding in &submission.findings {
                if let Some(ref location) = finding.location {
                    let entry = verdicts_by_file.entry(location.file.clone()).or_default();
                    entry.push((finding.description.clone(), submission.verdict));
                }
            }
        }

        for (file, verdicts) in verdicts_by_file {
            let has_approves = verdicts.iter().any(|(_, v)| *v == ReviewVerdict::Approve);
            let has_rejects = verdicts.iter().any(|(_, v)| *v == ReviewVerdict::Reject);
            let has_changes = verdicts
                .iter()
                .any(|(_, v)| *v == ReviewVerdict::ChangesRequested);

            if has_approves && (has_rejects || has_changes) {
                let descriptions: Vec<&str> = verdicts.iter().map(|(d, _)| d.as_str()).collect();
                dissent.push(format!(
                    "Dissent on {}: reviewers disagree - '{}'",
                    file,
                    descriptions.join("', '")
                ));
            }
        }

        let verdicts: Vec<ReviewVerdict> = submissions.iter().map(|s| s.verdict).collect();
        let has_approves = verdicts.contains(&ReviewVerdict::Approve);
        let has_rejects = verdicts.contains(&ReviewVerdict::Reject);

        if has_approves && has_rejects {
            dissent.push(
                "Reviewers have conflicting verdicts (some approve, some reject)".to_string(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::ReviewerSubmission;
    use brehon_types::{InlineComment, SessionId};

    fn make_finding(
        description: &str,
        file: &str,
        line: u32,
        severity: CommentSeverity,
    ) -> ReviewFinding {
        ReviewFinding {
            description: description.to_string(),
            location: Some(InlineComment {
                file: file.to_string(),
                line,
                content: description.to_string(),
                severity,
            }),
            suggestion: None,
            severity,
        }
    }

    fn make_submission(findings: Vec<ReviewFinding>, verdict: ReviewVerdict) -> ReviewerSubmission {
        ReviewerSubmission {
            reviewer_id: "reviewer".to_string(),
            session_id: SessionId::new("session"),
            score: brehon_types::ReviewScore::new(7),
            verdict,
            findings,
        }
    }

    #[test]
    fn consolidate_groups_similar_findings() {
        let consolidator = FeedbackConsolidator::new();

        let findings1 = vec![make_finding(
            "Error handling in auth.rs",
            "auth.rs",
            42,
            CommentSeverity::Blocking,
        )];
        let findings2 = vec![make_finding(
            "Error handling in auth.rs",
            "auth.rs",
            42,
            CommentSeverity::Blocking,
        )];

        let submissions = vec![
            make_submission(findings1, ReviewVerdict::ChangesRequested),
            make_submission(findings2, ReviewVerdict::ChangesRequested),
        ];

        let consolidated = consolidator.consolidate(&submissions);

        assert_eq!(consolidated.blocking.len(), 1);
        assert!(consolidated.blocking[0]
            .description
            .contains("Error handling"));
    }

    #[test]
    fn consolidate_separates_blocking_and_suggestions() {
        let consolidator = FeedbackConsolidator::new();

        let findings = vec![
            make_finding("Critical bug", "file.rs", 10, CommentSeverity::Blocking),
            make_finding(
                "Minor improvement",
                "file.rs",
                20,
                CommentSeverity::Suggestion,
            ),
            make_finding("Style issue", "file.rs", 30, CommentSeverity::Nitpick),
        ];

        let submissions = vec![make_submission(findings, ReviewVerdict::ChangesRequested)];

        let consolidated = consolidator.consolidate(&submissions);

        assert_eq!(consolidated.blocking.len(), 1);
        assert_eq!(consolidated.suggestions.len(), 1);
        assert_eq!(consolidated.nitpicks.len(), 1);
    }

    #[test]
    fn consolidate_preserves_dissent() {
        let consolidator = FeedbackConsolidator::new();

        let findings1 = vec![make_finding(
            "Security issue",
            "auth.rs",
            10,
            CommentSeverity::Blocking,
        )];
        let findings2 = vec![make_finding(
            "This is fine",
            "auth.rs",
            10,
            CommentSeverity::Suggestion,
        )];

        let submissions = vec![
            ReviewerSubmission {
                reviewer_id: "r1".to_string(),
                session_id: SessionId::new("s1"),
                score: brehon_types::ReviewScore::new(4),
                verdict: ReviewVerdict::Reject,
                findings: findings1,
            },
            ReviewerSubmission {
                reviewer_id: "r2".to_string(),
                session_id: SessionId::new("s2"),
                score: brehon_types::ReviewScore::new(9),
                verdict: ReviewVerdict::Approve,
                findings: findings2,
            },
        ];

        let consolidated = consolidator.consolidate(&submissions);

        assert!(!consolidated.dissent.is_empty());
    }

    #[test]
    fn consolidate_no_findings() {
        let consolidator = FeedbackConsolidator::new();

        let submissions = vec![make_submission(vec![], ReviewVerdict::Approve)];

        let consolidated = consolidator.consolidate(&submissions);

        assert!(consolidated.blocking.is_empty());
        assert!(consolidated.suggestions.is_empty());
        assert!(consolidated.nitpicks.is_empty());
        assert!(consolidated.dissent.is_empty());
    }

    #[test]
    fn consolidate_preserves_different_file_findings() {
        let consolidator = FeedbackConsolidator::new();

        let findings1 = vec![
            make_finding("Issue 1", "file1.rs", 10, CommentSeverity::Blocking),
            make_finding("Issue 2", "file2.rs", 20, CommentSeverity::Blocking),
        ];
        let findings2 = vec![make_finding(
            "Issue 3",
            "file3.rs",
            30,
            CommentSeverity::Blocking,
        )];

        let submissions = vec![
            make_submission(findings1, ReviewVerdict::ChangesRequested),
            make_submission(findings2, ReviewVerdict::ChangesRequested),
        ];

        let consolidated = consolidator.consolidate(&submissions);

        assert_eq!(consolidated.blocking.len(), 3);
    }
}

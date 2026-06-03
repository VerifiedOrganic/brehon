//! Score collection and threshold evaluation.
//!
//! Handles:
//! - Collecting scores from reviewers
//! - Evaluating against policy thresholds
//! - Determining review outcomes

use std::collections::HashMap;

use brehon_types::{ReviewPolicy, ReviewScore, ReviewVerdict};

/// Result of threshold evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThresholdResult {
    /// Review approved - all thresholds met.
    Approved,
    /// Changes requested - blocking issues or low scores.
    ChangesRequested,
    /// Rejected - fundamental issues.
    Rejected,
    /// More reviewers needed.
    NeedMoreReviewers,
}

impl ThresholdResult {
    pub fn is_approved(&self) -> bool {
        matches!(self, ThresholdResult::Approved)
    }

    pub fn is_rejected(&self) -> bool {
        matches!(self, ThresholdResult::Rejected)
    }

    pub fn needs_changes(&self) -> bool {
        matches!(self, ThresholdResult::ChangesRequested)
    }
}

/// Collector for gathering and aggregating review scores.
#[derive(Debug, Clone)]
pub struct ScoreCollector {
    scores: HashMap<String, (ReviewScore, ReviewVerdict)>,
}

impl Default for ScoreCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl ScoreCollector {
    pub fn new() -> Self {
        Self {
            scores: HashMap::new(),
        }
    }

    pub fn add(&mut self, reviewer_id: String, score: ReviewScore, verdict: ReviewVerdict) {
        self.scores.insert(reviewer_id, (score, verdict));
    }

    pub fn scores(&self) -> &HashMap<String, (ReviewScore, ReviewVerdict)> {
        &self.scores
    }

    pub fn count(&self) -> usize {
        self.scores.len()
    }

    pub fn average_score(&self) -> Option<f64> {
        if self.scores.is_empty() {
            return None;
        }

        let total: u64 = self.scores.values().map(|(s, _)| s.as_u8() as u64).sum();
        Some(total as f64 / self.scores.len() as f64)
    }

    pub fn min_score(&self) -> Option<ReviewScore> {
        self.scores
            .values()
            .map(|(s, _)| s.as_u8())
            .min()
            .map(|s| ReviewScore::try_from(s).unwrap())
    }

    pub fn max_score(&self) -> Option<ReviewScore> {
        self.scores
            .values()
            .map(|(s, _)| s.as_u8())
            .max()
            .map(|s| ReviewScore::try_from(s).unwrap())
    }

    pub fn approval_count(&self) -> usize {
        self.scores
            .values()
            .filter(|(_, v)| *v == ReviewVerdict::Approve)
            .count()
    }

    pub fn has_rejection(&self) -> bool {
        self.scores
            .values()
            .any(|(_, v)| *v == ReviewVerdict::Reject)
    }

    pub fn has_blocking_findings(&self) -> bool {
        self.scores.values().any(|(s, _)| s.as_u8() <= 5)
    }

    pub fn clear(&mut self) {
        self.scores.clear();
    }
}

/// Evaluates scores against review policy thresholds.
#[derive(Debug, Clone)]
pub struct ThresholdEvaluator {
    policy: ReviewPolicy,
}

impl ThresholdEvaluator {
    pub fn new(policy: ReviewPolicy) -> Self {
        Self { policy }
    }

    pub fn policy(&self) -> &ReviewPolicy {
        &self.policy
    }

    /// Evaluate collected scores against policy thresholds.
    ///
    /// Rules applied in order:
    /// 1. Any single score <= 3 or Reject verdict → Rejected
    /// 2. Any score <= blocking_score → ChangesRequested
    /// 3. Approvals < min_approvals → NeedMoreReviewers
    /// 4. Min score < min_individual_score → ChangesRequested
    /// 5. Average < min_average_score → ChangesRequested
    /// 6. Otherwise → Approved
    pub fn evaluate(&self, collector: &ScoreCollector) -> ThresholdResult {
        let scores: Vec<_> = collector.scores().values().collect();

        if scores.is_empty() {
            return ThresholdResult::NeedMoreReviewers;
        }

        for (score, verdict) in &scores {
            if score.as_u8() <= 3 || *verdict == ReviewVerdict::Reject {
                return ThresholdResult::Rejected;
            }
        }

        for (score, verdict) in &scores {
            if score.as_u8() <= self.policy.blocking_score {
                return ThresholdResult::ChangesRequested;
            }
            if *verdict == ReviewVerdict::ChangesRequested {
                return ThresholdResult::ChangesRequested;
            }
        }

        let approval_count = collector.approval_count();
        if (approval_count as u8) < self.policy.min_approvals {
            return ThresholdResult::NeedMoreReviewers;
        }

        if let Some(min_score) = collector.min_score() {
            if min_score.as_u8() < self.policy.min_individual_score {
                return ThresholdResult::ChangesRequested;
            }
        }

        if let Some(avg) = collector.average_score() {
            if avg < self.policy.min_average_score as f64 {
                return ThresholdResult::ChangesRequested;
            }
        }

        ThresholdResult::Approved
    }

    /// Evaluate with a specific number of reviewers (for panel size decisions).
    pub fn evaluate_with_reviewer_count(
        &self,
        collector: &ScoreCollector,
        reviewer_count: usize,
    ) -> ThresholdResult {
        if collector.count() < reviewer_count {
            return ThresholdResult::NeedMoreReviewers;
        }
        self.evaluate(collector)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_policy() -> ReviewPolicy {
        ReviewPolicy {
            min_average_score: 7,
            min_individual_score: 6,
            blocking_score: 5,
            min_approvals: 2,
            require_blocking_feedback_resolution: true,
            max_review_rounds: 3,
        }
    }

    #[test]
    fn score_collector_average() {
        let mut collector = ScoreCollector::new();
        collector.add(
            "r1".to_string(),
            ReviewScore::new(8),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r2".to_string(),
            ReviewScore::new(7),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r3".to_string(),
            ReviewScore::new(9),
            ReviewVerdict::Approve,
        );

        let avg = collector.average_score().unwrap();
        assert!((avg - 8.0).abs() < 0.01);
    }

    #[test]
    fn score_collector_min_max() {
        let mut collector = ScoreCollector::new();
        collector.add(
            "r1".to_string(),
            ReviewScore::new(5),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r2".to_string(),
            ReviewScore::new(9),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r3".to_string(),
            ReviewScore::new(7),
            ReviewVerdict::Approve,
        );

        assert_eq!(collector.min_score().unwrap().as_u8(), 5);
        assert_eq!(collector.max_score().unwrap().as_u8(), 9);
    }

    #[test]
    fn score_collector_approval_count() {
        let mut collector = ScoreCollector::new();
        collector.add(
            "r1".to_string(),
            ReviewScore::new(8),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r2".to_string(),
            ReviewScore::new(7),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r3".to_string(),
            ReviewScore::new(6),
            ReviewVerdict::ChangesRequested,
        );

        assert_eq!(collector.approval_count(), 2);
    }

    #[test]
    fn score_collector_has_rejection() {
        let mut collector = ScoreCollector::new();

        collector.add(
            "r1".to_string(),
            ReviewScore::new(8),
            ReviewVerdict::Approve,
        );
        assert!(!collector.has_rejection());

        collector.add("r2".to_string(), ReviewScore::new(3), ReviewVerdict::Reject);
        assert!(collector.has_rejection());
    }

    #[test]
    fn threshold_evaluator_approved() {
        let evaluator = ThresholdEvaluator::new(default_policy());
        let mut collector = ScoreCollector::new();

        collector.add(
            "r1".to_string(),
            ReviewScore::new(8),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r2".to_string(),
            ReviewScore::new(7),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r3".to_string(),
            ReviewScore::new(9),
            ReviewVerdict::Approve,
        );

        let result = evaluator.evaluate(&collector);
        assert_eq!(result, ThresholdResult::Approved);
    }

    #[test]
    fn threshold_evaluator_rejected_score_too_low() {
        let evaluator = ThresholdEvaluator::new(default_policy());
        let mut collector = ScoreCollector::new();

        collector.add(
            "r1".to_string(),
            ReviewScore::new(8),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r2".to_string(),
            ReviewScore::new(7),
            ReviewVerdict::Approve,
        );
        collector.add("r3".to_string(), ReviewScore::new(3), ReviewVerdict::Reject);

        let result = evaluator.evaluate(&collector);
        assert_eq!(result, ThresholdResult::Rejected);
    }

    #[test]
    fn threshold_evaluator_rejected_verdict() {
        let evaluator = ThresholdEvaluator::new(default_policy());
        let mut collector = ScoreCollector::new();

        collector.add(
            "r1".to_string(),
            ReviewScore::new(8),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r2".to_string(),
            ReviewScore::new(7),
            ReviewVerdict::Approve,
        );
        collector.add("r3".to_string(), ReviewScore::new(4), ReviewVerdict::Reject);

        let result = evaluator.evaluate(&collector);
        assert_eq!(result, ThresholdResult::Rejected);
    }

    #[test]
    fn threshold_evaluator_changes_requested_blocking_score() {
        let evaluator = ThresholdEvaluator::new(default_policy());
        let mut collector = ScoreCollector::new();

        collector.add(
            "r1".to_string(),
            ReviewScore::new(8),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r2".to_string(),
            ReviewScore::new(7),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r3".to_string(),
            ReviewScore::new(5),
            ReviewVerdict::ChangesRequested,
        );

        let result = evaluator.evaluate(&collector);
        assert_eq!(result, ThresholdResult::ChangesRequested);
    }

    #[test]
    fn threshold_evaluator_changes_requested_low_individual() {
        let evaluator = ThresholdEvaluator::new(default_policy());
        let mut collector = ScoreCollector::new();

        collector.add(
            "r1".to_string(),
            ReviewScore::new(8),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r2".to_string(),
            ReviewScore::new(7),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r3".to_string(),
            ReviewScore::new(5),
            ReviewVerdict::Approve,
        );

        let result = evaluator.evaluate(&collector);
        assert_eq!(result, ThresholdResult::ChangesRequested);
    }

    #[test]
    fn threshold_evaluator_changes_requested_low_average() {
        let evaluator = ThresholdEvaluator::new(default_policy());
        let mut collector = ScoreCollector::new();

        collector.add(
            "r1".to_string(),
            ReviewScore::new(7),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r2".to_string(),
            ReviewScore::new(6),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r3".to_string(),
            ReviewScore::new(6),
            ReviewVerdict::Approve,
        );

        let result = evaluator.evaluate(&collector);
        let avg = collector.average_score().unwrap();
        assert!((avg - 6.33).abs() < 0.1);
        assert_eq!(result, ThresholdResult::ChangesRequested);
    }

    #[test]
    fn threshold_evaluator_need_more_reviewers() {
        let evaluator = ThresholdEvaluator::new(default_policy());
        let mut collector = ScoreCollector::new();

        collector.add(
            "r1".to_string(),
            ReviewScore::new(8),
            ReviewVerdict::Approve,
        );

        let result = evaluator.evaluate(&collector);
        assert_eq!(result, ThresholdResult::NeedMoreReviewers);

        let result = evaluator.evaluate_with_reviewer_count(&collector, 3);
        assert_eq!(result, ThresholdResult::NeedMoreReviewers);
    }

    #[test]
    fn threshold_evaluator_average_8_with_policy_7_approved() {
        let evaluator = ThresholdEvaluator::new(default_policy());
        let mut collector = ScoreCollector::new();

        collector.add(
            "r1".to_string(),
            ReviewScore::new(8),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r2".to_string(),
            ReviewScore::new(8),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r3".to_string(),
            ReviewScore::new(8),
            ReviewVerdict::Approve,
        );

        let avg = collector.average_score().unwrap();
        assert!((avg - 8.0).abs() < 0.01);
        assert_eq!(collector.min_score().unwrap().as_u8(), 8);
        assert_eq!(collector.approval_count(), 3);

        let result = evaluator.evaluate(&collector);
        assert_eq!(result, ThresholdResult::Approved);
    }

    #[test]
    fn threshold_evaluator_average_63_with_score_5_changes_requested() {
        let evaluator = ThresholdEvaluator::new(default_policy());
        let mut collector = ScoreCollector::new();

        collector.add(
            "r1".to_string(),
            ReviewScore::new(7),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r2".to_string(),
            ReviewScore::new(7),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r3".to_string(),
            ReviewScore::new(5),
            ReviewVerdict::ChangesRequested,
        );

        let avg = collector.average_score().unwrap();
        assert!((avg - 6.33).abs() < 0.01);

        let result = evaluator.evaluate(&collector);
        assert_eq!(result, ThresholdResult::ChangesRequested);
    }

    #[test]
    fn threshold_evaluator_any_score_3_rejected() {
        let evaluator = ThresholdEvaluator::new(default_policy());
        let mut collector = ScoreCollector::new();

        collector.add(
            "r1".to_string(),
            ReviewScore::new(9),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r2".to_string(),
            ReviewScore::new(8),
            ReviewVerdict::Approve,
        );
        collector.add("r3".to_string(), ReviewScore::new(3), ReviewVerdict::Reject);

        let result = evaluator.evaluate(&collector);
        assert_eq!(result, ThresholdResult::Rejected);
    }

    #[test]
    fn threshold_evaluator_reject_verdict_rejected() {
        let evaluator = ThresholdEvaluator::new(default_policy());
        let mut collector = ScoreCollector::new();

        collector.add(
            "r1".to_string(),
            ReviewScore::new(8),
            ReviewVerdict::Approve,
        );
        collector.add(
            "r2".to_string(),
            ReviewScore::new(7),
            ReviewVerdict::Approve,
        );
        collector.add("r3".to_string(), ReviewScore::new(6), ReviewVerdict::Reject);

        let result = evaluator.evaluate(&collector);
        assert_eq!(result, ThresholdResult::Rejected);
    }

    #[test]
    fn threshold_result_is_approved() {
        assert!(ThresholdResult::Approved.is_approved());
        assert!(!ThresholdResult::ChangesRequested.is_approved());
        assert!(!ThresholdResult::Rejected.is_approved());
        assert!(!ThresholdResult::NeedMoreReviewers.is_approved());
    }

    #[test]
    fn threshold_result_is_rejected() {
        assert!(!ThresholdResult::Approved.is_rejected());
        assert!(!ThresholdResult::ChangesRequested.is_rejected());
        assert!(ThresholdResult::Rejected.is_rejected());
        assert!(!ThresholdResult::NeedMoreReviewers.is_rejected());
    }

    #[test]
    fn threshold_result_needs_changes() {
        assert!(!ThresholdResult::Approved.needs_changes());
        assert!(ThresholdResult::ChangesRequested.needs_changes());
        assert!(!ThresholdResult::Rejected.needs_changes());
        assert!(!ThresholdResult::NeedMoreReviewers.needs_changes());
    }
}

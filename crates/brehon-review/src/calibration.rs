//! Per-reviewer statistics for calibration.
//!
//! Tracks:
//! - Average score
//! - Standard deviation
//! - Approval rate
//! - Outlier flagging

use std::collections::HashMap;

/// Statistics for a single reviewer.
#[derive(Debug, Clone)]
pub struct PerReviewerStats {
    /// Number of reviews completed.
    pub review_count: u32,
    /// Sum of all scores (for average calculation).
    pub score_sum: u64,
    /// Sum of squared scores (for std dev calculation).
    pub score_squared_sum: u64,
    /// Number of approvals.
    pub approval_count: u32,
    /// Number of rejections.
    pub rejection_count: u32,
    /// Number of changes requested.
    pub changes_requested_count: u32,
}

impl Default for PerReviewerStats {
    fn default() -> Self {
        Self::new()
    }
}

impl PerReviewerStats {
    pub fn new() -> Self {
        Self {
            review_count: 0,
            score_sum: 0,
            score_squared_sum: 0,
            approval_count: 0,
            rejection_count: 0,
            changes_requested_count: 0,
        }
    }

    pub fn add_review(&mut self, score: u8, is_approval: bool, is_rejection: bool) {
        self.review_count += 1;
        self.score_sum += score as u64;
        self.score_squared_sum += (score as u64) * (score as u64);

        if is_approval {
            self.approval_count += 1;
        }
        if is_rejection {
            self.rejection_count += 1;
        } else if !is_approval {
            self.changes_requested_count += 1;
        }
    }

    pub fn average_score(&self) -> Option<f64> {
        if self.review_count == 0 {
            return None;
        }
        Some(self.score_sum as f64 / self.review_count as f64)
    }

    pub fn std_deviation(&self) -> Option<f64> {
        if self.review_count < 2 {
            return None;
        }

        let mean = self.score_sum as f64 / self.review_count as f64;
        let variance = (self.score_squared_sum as f64 / self.review_count as f64) - (mean * mean);

        if variance < 0.0 {
            return None;
        }

        Some(variance.sqrt())
    }

    pub fn approval_rate(&self) -> Option<f64> {
        if self.review_count == 0 {
            return None;
        }
        Some(self.approval_count as f64 / self.review_count as f64)
    }

    pub fn is_outlier(&self, global_avg: f64, threshold: f64) -> bool {
        if let Some(avg) = self.average_score() {
            let diff = (avg - global_avg).abs();
            if let Some(std) = self.std_deviation() {
                return std > threshold || diff > threshold;
            }
            return diff > threshold;
        }
        false
    }
}

/// Tracks statistics across all reviewers.
pub struct ReviewerCalibration {
    stats: HashMap<String, PerReviewerStats>,
    outlier_threshold: f64,
}

impl Default for ReviewerCalibration {
    fn default() -> Self {
        Self::new()
    }
}

impl ReviewerCalibration {
    pub fn new() -> Self {
        Self {
            stats: HashMap::new(),
            outlier_threshold: 2.0,
        }
    }

    pub fn with_threshold(threshold: f64) -> Self {
        Self {
            stats: HashMap::new(),
            outlier_threshold: threshold,
        }
    }

    pub fn record_review(
        &mut self,
        reviewer_id: &str,
        score: u8,
        is_approval: bool,
        is_rejection: bool,
    ) {
        let entry = self.stats.entry(reviewer_id.to_string()).or_default();
        entry.add_review(score, is_approval, is_rejection);
    }

    pub fn get_stats(&self, reviewer_id: &str) -> Option<&PerReviewerStats> {
        self.stats.get(reviewer_id)
    }

    pub fn all_reviewers(&self) -> impl Iterator<Item = (&String, &PerReviewerStats)> {
        self.stats.iter()
    }

    pub fn global_average(&self) -> Option<f64> {
        let total_count: u64 = self.stats.values().map(|s| s.review_count as u64).sum();

        if total_count == 0 {
            return None;
        }

        let total_score: u64 = self.stats.values().map(|s| s.score_sum).sum();
        Some(total_score as f64 / total_count as f64)
    }

    pub fn find_outliers(&self) -> Vec<(String, f64, PerReviewerStats)> {
        let global_avg = match self.global_average() {
            Some(avg) => avg,
            None => return vec![],
        };

        self.stats
            .iter()
            .filter(|(_, stats)| stats.is_outlier(global_avg, self.outlier_threshold))
            .map(|(id, stats)| {
                let avg = stats.average_score().unwrap_or(0.0);
                (id.clone(), avg, stats.clone())
            })
            .collect()
    }

    pub fn global_approval_rate(&self) -> Option<f64> {
        let total_count: u32 = self.stats.values().map(|s| s.review_count).sum();

        if total_count == 0 {
            return None;
        }

        let total_approvals: u32 = self.stats.values().map(|s| s.approval_count).sum();
        Some(total_approvals as f64 / total_count as f64)
    }

    pub fn global_std_deviation(&self) -> Option<f64> {
        let total_count: u64 = self.stats.values().map(|s| s.review_count as u64).sum();

        if total_count < 2 {
            return None;
        }

        let global_avg = self.global_average()?;
        let _total_score: u64 = self.stats.values().map(|s| s.score_sum).sum();
        let total_squared: u64 = self.stats.values().map(|s| s.score_squared_sum).sum();

        let variance = (total_squared as f64 / total_count as f64) - (global_avg * global_avg);

        if variance < 0.0 {
            return None;
        }

        Some(variance.sqrt())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_reviewer_stats_basic() {
        let mut stats = PerReviewerStats::new();

        stats.add_review(8, true, false);
        stats.add_review(7, true, false);
        stats.add_review(9, true, false);

        assert_eq!(stats.review_count, 3);
        let avg = stats.average_score().unwrap();
        assert!((avg - 8.0).abs() < 0.01);

        let std = stats.std_deviation().unwrap();
        assert!(std > 0.0);

        let rate = stats.approval_rate().unwrap();
        assert!((rate - 1.0).abs() < 0.01);
    }

    #[test]
    fn per_reviewer_stats_mixed() {
        let mut stats = PerReviewerStats::new();

        stats.add_review(8, true, false);
        stats.add_review(5, false, false);
        stats.add_review(3, false, true);

        assert_eq!(stats.review_count, 3);
        assert_eq!(stats.approval_count, 1);
        assert_eq!(stats.rejection_count, 1);
        assert_eq!(stats.changes_requested_count, 1);

        let rate = stats.approval_rate().unwrap();
        assert!((rate - 0.333).abs() < 0.01);
    }

    #[test]
    fn per_reviewer_stats_outlier() {
        let mut stats = PerReviewerStats::new();

        for _ in 0..10 {
            stats.add_review(8, true, false);
        }

        let avg = stats.average_score().unwrap();
        assert!((avg - 8.0).abs() < 0.01);

        let std = stats.std_deviation().unwrap();
        assert!(std < 1.0);

        assert!(!stats.is_outlier(7.0, 2.0));

        let mut outlier_stats = PerReviewerStats::new();
        for _ in 0..10 {
            outlier_stats.add_review(3, false, false);
        }
        assert!(outlier_stats.is_outlier(7.0, 2.0));
    }

    #[test]
    fn calibration_global_stats() {
        let mut calibration = ReviewerCalibration::new();

        calibration.record_review("reviewer-1", 8, true, false);
        calibration.record_review("reviewer-1", 7, true, false);
        calibration.record_review("reviewer-2", 9, true, false);
        calibration.record_review("reviewer-2", 8, true, false);
        calibration.record_review("reviewer-3", 4, false, false);

        let global_avg = calibration.global_average().unwrap();
        assert!((global_avg - 7.2).abs() < 0.1);

        let stats1 = calibration.get_stats("reviewer-1").unwrap();
        assert_eq!(stats1.review_count, 2);

        let outliers = calibration.find_outliers();
        assert!(!outliers.is_empty());
    }

    #[test]
    fn calibration_after_10_reviews() {
        let mut calibration = ReviewerCalibration::new();

        for i in 1..=10 {
            let score = 7 + (i % 3);
            calibration.record_review(&format!("reviewer-{}", i), score, true, false);
        }

        let stats = calibration.get_stats("reviewer-1").unwrap();
        assert_eq!(stats.review_count, 1);

        let _avg = stats.average_score().unwrap();

        let _calibrated = ReviewerCalibration::new();
        let mut reviewer_stats = PerReviewerStats::new();
        for i in 0..10 {
            reviewer_stats.add_review(7 + (i % 3), true, false);
        }

        let calculated_avg = reviewer_stats.average_score().unwrap();
        let calculated_std = reviewer_stats.std_deviation().unwrap();

        assert!((calculated_avg - 7.9).abs() < 0.5);
        assert!(calculated_std < 1.5);
    }

    #[test]
    fn calibration_empty() {
        let calibration = ReviewerCalibration::new();

        assert!(calibration.global_average().is_none());
        assert!(calibration.global_std_deviation().is_none());
        assert!(calibration.get_stats("unknown").is_none());
        let outliers = calibration.find_outliers();
        assert!(outliers.is_empty());
    }
}

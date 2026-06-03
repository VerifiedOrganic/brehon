//! Mock behavior configuration for scripted agent responses.

use std::time::Duration;

/// Configuration for mock agent behavior.
///
/// Use this to script agent responses for testing scenarios.
#[derive(Debug, Clone)]
pub struct MockBehavior {
    pub response_delay: Duration,
    pub stuck_after_message: Option<usize>,
    pub crash_after_message: Option<usize>,
    pub review_scores: Vec<u8>,
    pub progress_events: Vec<String>,
    pub response_content: Option<String>,
    pub max_responses: Option<usize>,
    pub responses_made: usize,
}

impl Default for MockBehavior {
    fn default() -> Self {
        Self {
            response_delay: Duration::ZERO,
            stuck_after_message: None,
            crash_after_message: None,
            review_scores: vec![],
            progress_events: vec![],
            response_content: None,
            max_responses: None,
            responses_made: 0,
        }
    }
}

impl MockBehavior {
    pub fn normal() -> Self {
        Self::default()
    }

    pub fn stuck_after(n: usize) -> Self {
        Self {
            stuck_after_message: Some(n),
            ..Self::default()
        }
    }

    pub fn crashing_after(n: usize) -> Self {
        Self {
            crash_after_message: Some(n),
            ..Self::default()
        }
    }

    pub fn reviewer(scores: Vec<u8>) -> Self {
        Self {
            review_scores: scores,
            ..Self::default()
        }
    }

    pub fn with_delay(delay: Duration) -> Self {
        Self {
            response_delay: delay,
            ..Self::default()
        }
    }

    pub fn with_responses(max: usize, content: impl Into<String>) -> Self {
        Self {
            max_responses: Some(max),
            response_content: Some(content.into()),
            ..Self::default()
        }
    }

    pub fn with_progress(events: Vec<String>) -> Self {
        Self {
            progress_events: events,
            ..Self::default()
        }
    }

    pub fn is_stuck(&self, messages_sent: usize) -> bool {
        self.stuck_after_message.is_some_and(|n| messages_sent >= n)
    }

    pub fn next_review_score(&mut self) -> Option<u8> {
        if self.review_scores.is_empty() {
            Some(7)
        } else if self.review_scores.len() == 1 {
            Some(self.review_scores[0])
        } else {
            let idx = self.responses_made.min(self.review_scores.len() - 1);
            Some(self.review_scores[idx])
        }
    }

    pub fn can_respond(&self) -> bool {
        if let Some(max) = self.max_responses {
            self.responses_made < max
        } else {
            true
        }
    }

    pub fn get_progress_event(&self, idx: usize) -> Option<&str> {
        self.progress_events.get(idx).map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_behavior() {
        let b = MockBehavior::normal();
        assert!(b.stuck_after_message.is_none());
        assert!(b.crash_after_message.is_none());
        assert_eq!(b.response_delay, Duration::ZERO);
    }

    #[test]
    fn stuck_detection() {
        let b = MockBehavior::stuck_after(3);
        assert!(!b.is_stuck(2));
        assert!(b.is_stuck(3));
        assert!(b.is_stuck(4));
    }

    #[test]
    fn reviewer_scores() {
        let mut b = MockBehavior::reviewer(vec![8, 7, 9]);
        assert_eq!(b.next_review_score(), Some(8));
        b.responses_made += 1;
        assert_eq!(b.next_review_score(), Some(7));
        b.responses_made += 1;
        assert_eq!(b.next_review_score(), Some(9));
    }

    #[test]
    fn max_responses() {
        let b = MockBehavior::with_responses(3, "done");
        assert!(b.can_respond());
        let mut b = b.clone();
        b.responses_made = 3;
        assert!(!b.can_respond());
    }
}

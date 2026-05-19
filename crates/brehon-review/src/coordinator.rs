//! Main Review Coordinator.
//!
//! Coordinates:
//! - Queue processing (priority lanes, FIFO within lane)
//! - Panel management (spawning, affinity, teardown)
//! - Score collection and threshold evaluation
//! - Stale detection
//! - Session lifecycle

use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use brehon_mux::{Mux, ReviewContextSnapshot};
use brehon_ports::{AgentGateway, EventStore, GitOperations};
use brehon_types::{
    config::ReviewConfig, ClaimId, CommentSeverity, Event, EventKind, ReviewId, ReviewPolicy,
    ReviewVerdict, StabilityCounters,
};
use tokio::sync::Mutex;

use crate::calibration::ReviewerCalibration;
use crate::chunking::ChunkingConfig;
use crate::lifecycle::{ReviewLifecycle, ReviewOutcome};
use crate::panel::{PanelAffinity, ReviewPanel, ReviewerSubmission};
use crate::queue::PriorityQueue;
use crate::scoring::ScoreCollector;
use crate::stale::{StaleDetection, StaleDetector};
use crate::ReviewError;

/// Configuration for the review coordinator.
#[derive(Debug, Clone)]
pub struct CoordinatorConfig {
    /// Number of reviewers per panel.
    pub panel_size: usize,
    /// Default reviewers to use.
    pub default_reviewers: Vec<String>,
    /// Lease duration for queue claims.
    pub claim_lease_duration: Duration,
    /// Interval between polling for new reviews.
    pub poll_interval: Duration,
    /// Review policy.
    pub policy: ReviewPolicy,
    /// Chunking configuration.
    pub chunking: ChunkingConfig,
    /// Stale detection enabled.
    pub stale_detection_enabled: bool,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            panel_size: 3,
            default_reviewers: vec![],
            claim_lease_duration: Duration::from_secs(300),
            poll_interval: Duration::from_secs(5),
            policy: ReviewPolicy::default(),
            chunking: ChunkingConfig::default(),
            stale_detection_enabled: true,
        }
    }
}

impl CoordinatorConfig {
    pub fn from_review_config(config: &ReviewConfig) -> Self {
        Self {
            panel_size: 3,
            default_reviewers: config.default_reviewers.clone(),
            claim_lease_duration: Duration::from_secs(300),
            poll_interval: Duration::from_secs(5),
            policy: config.policy.clone(),
            chunking: ChunkingConfig::new(config.max_diff_tokens, config.chunk_strategy),
            stale_detection_enabled: config.stale_detection.enabled,
        }
    }
}

/// Active review state.
#[allow(dead_code)]
struct ActiveReview {
    review_id: ReviewId,
    task_id: String,
    branch: String,
    base_commit: String,
    claim_id: ClaimId,
    panel: ReviewPanel,
    round: u32,
    collector: ScoreCollector,
    chunks: Vec<crate::chunking::DiffChunk>,
    current_chunk: usize,
}

#[derive(Debug, Clone)]
enum ReviewContextTarget {
    PaneId(String),
    SessionId(String),
}

type ReviewContextSink =
    Arc<dyn Fn(ReviewContextTarget, Option<ReviewContextSnapshot>) + Send + Sync>;

/// The main Review Coordinator.
///
/// Responsible for:
/// - Processing reviews from the queue in priority order
/// - Managing reviewer panels with affinity
/// - Collecting and evaluating scores
/// - Handling stale review detection
/// - Managing review session lifecycle
#[allow(dead_code)]
pub struct ReviewCoordinator {
    config: CoordinatorConfig,
    store: Arc<dyn EventStore>,
    gateway: Arc<dyn AgentGateway>,
    git: Arc<dyn GitOperations>,
    queue: PriorityQueue,
    lifecycle: ReviewLifecycle,
    calibration: ReviewerCalibration,
    stale_detector: Option<StaleDetector>,
    active_reviews: Vec<ActiveReview>,
    review_context_sink: Option<ReviewContextSink>,
}

impl ReviewCoordinator {
    pub fn new(
        config: CoordinatorConfig,
        store: Arc<dyn EventStore>,
        gateway: Arc<dyn AgentGateway>,
        git: Arc<dyn GitOperations>,
    ) -> Self {
        let queue =
            PriorityQueue::new(store.clone()).with_lease_duration(config.claim_lease_duration);
        let lifecycle = ReviewLifecycle::new(config.policy.clone(), store.clone());
        let stale_detector = if config.stale_detection_enabled {
            Some(StaleDetector::new(git.clone()))
        } else {
            None
        };

        Self {
            config,
            store,
            gateway,
            git,
            queue,
            lifecycle,
            calibration: ReviewerCalibration::new(),
            stale_detector,
            active_reviews: Vec::new(),
            review_context_sink: None,
        }
    }

    pub fn with_calibration(mut self, calibration: ReviewerCalibration) -> Self {
        self.calibration = calibration;
        self
    }

    pub fn with_mux(mut self, mux: Arc<Mutex<Mux>>) -> Self {
        let sink: ReviewContextSink = Arc::new(move |target, context| {
            let mux = mux.clone();
            // Fire-and-forget delivery into shared mux state. Review context is
            // UI metadata and must not block coordinator state transitions.
            tokio::spawn(async move {
                let mut mux = mux.lock().await;
                match (target, context) {
                    (ReviewContextTarget::PaneId(pane_id), Some(snapshot)) => {
                        mux.set_pane_review_context(&pane_id, snapshot);
                    }
                    (ReviewContextTarget::PaneId(pane_id), None) => {
                        mux.clear_pane_review_context(&pane_id);
                    }
                    (ReviewContextTarget::SessionId(session_id), Some(snapshot)) => {
                        mux.set_pane_review_context_by_session(&session_id, snapshot);
                    }
                    (ReviewContextTarget::SessionId(session_id), None) => {
                        mux.clear_pane_review_context_by_session(&session_id);
                    }
                }
            });
        });
        self.review_context_sink = Some(sink);
        self
    }

    #[cfg(test)]
    fn with_review_context_sink_for_test(mut self, sink: ReviewContextSink) -> Self {
        self.review_context_sink = Some(sink);
        self
    }

    /// Run the coordinator loop.
    pub async fn run(&mut self) -> Result<(), ReviewError> {
        loop {
            if let Err(e) = self.tick().await {
                warn!("Coordinator tick error: {}", e);
            }

            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    /// Shutdown the coordinator and terminate all active reviewer sessions.
    pub async fn shutdown(&mut self) -> Result<(), ReviewError> {
        let active_reviews = std::mem::take(&mut self.active_reviews);

        for review in &active_reviews {
            // Clear UI context before sessions are torn down.
            self.publish_review_context(review, None);
        }

        for review in active_reviews {
            self.record_calibration_data(&review);

            if let Err(e) = review.panel.kill_all_sessions().await {
                warn!(review_id = %review.review_id, error = %e, "Failed to terminate review sessions during shutdown");
            }
        }

        Ok(())
    }

    /// Single iteration of the coordinator loop.
    pub async fn tick(&mut self) -> Result<(), ReviewError> {
        self.check_active_reviews().await?;

        if let Some(claim) = self
            .queue
            .claim_next("review-coordinator")
            .await
            .map_err(ReviewError::Queue)?
        {
            self.start_review(claim.item_id.clone(), claim.claim_id.clone())
                .await?;
        }

        Ok(())
    }

    /// Check and update all active reviews.
    async fn check_active_reviews(&mut self) -> Result<(), ReviewError> {
        let mut outcomes: Vec<(usize, ReviewOutcome)> = Vec::new();

        for (idx, review) in self.active_reviews.iter().enumerate() {
            if review.panel.is_round_complete() {
                let result = self.evaluate_round(review).await;
                if let Ok(outcome) = result {
                    outcomes.push((idx, outcome));
                }
            }
        }

        let mut completed_indices: Vec<(usize, bool)> = Vec::new();

        for (idx, outcome) in outcomes {
            match outcome {
                ReviewOutcome::Approved => {
                    let finalized = Self::build_review_context_snapshot(
                        &self.active_reviews[idx],
                        Some(Self::verdict_label(ReviewVerdict::Approve)),
                        Self::summarize_round_findings_canonical(&self.active_reviews[idx]),
                    );
                    self.publish_review_context(&self.active_reviews[idx], Some(finalized));
                    completed_indices.push((idx, true));
                }
                ReviewOutcome::ChangesRequested { feedback } => {
                    let review_round = self.active_reviews[idx].round;
                    let review_id = self.active_reviews[idx].review_id.to_string();
                    let finalized = Self::build_review_context_snapshot(
                        &self.active_reviews[idx],
                        Some(Self::verdict_label(ReviewVerdict::ChangesRequested)),
                        Self::summarize_consolidated_feedback(&feedback),
                    );
                    self.publish_review_context(&self.active_reviews[idx], Some(finalized));

                    self.lifecycle.emit_changes_requested(&review_id).await?;
                    info!(
                        review_id = %review_id,
                        round = review_round,
                        blocking_issues = feedback.blocking.len(),
                        "Review round complete: changes requested"
                    );

                    if self.lifecycle.check_max_rounds(review_round) {
                        completed_indices.push((idx, false));
                    } else {
                        self.active_reviews[idx].round += 1;
                        self.active_reviews[idx].collector.clear();
                        let next_round = Self::build_review_context_snapshot(
                            &self.active_reviews[idx],
                            None,
                            None,
                        );
                        self.publish_review_context(&self.active_reviews[idx], Some(next_round));
                    }
                }
                ReviewOutcome::Rejected { feedback } => {
                    let review_id = self.active_reviews[idx].review_id.to_string();
                    let finalized = Self::build_review_context_snapshot(
                        &self.active_reviews[idx],
                        Some(Self::verdict_label(ReviewVerdict::Reject)),
                        Self::summarize_consolidated_feedback(&feedback),
                    );
                    self.publish_review_context(&self.active_reviews[idx], Some(finalized));
                    self.lifecycle.emit_rejection(&review_id).await?;
                    info!(review_id = %review_id, "Review rejected");
                    let _ = feedback;
                    completed_indices.push((idx, false));
                }
                ReviewOutcome::Escalate { reason, feedback } => {
                    let review_id = self.active_reviews[idx].review_id.to_string();
                    let finalized = Self::build_review_context_snapshot(
                        &self.active_reviews[idx],
                        Some("escalated".to_string()),
                        Self::summarize_consolidated_feedback(&feedback),
                    );
                    self.publish_review_context(&self.active_reviews[idx], Some(finalized));

                    let event = Event {
                        kind: EventKind::EscalationTriggered {
                            reason: reason.clone(),
                            context: format!("Review {}: max rounds exceeded", review_id),
                        },
                        timestamp: chrono::Utc::now(),
                        aggregate_id: review_id.clone(),
                    };

                    self.store
                        .append(event)
                        .await
                        .map_err(|e| ReviewError::Storage(e.to_string()))?;
                    warn!(review_id = %review_id, reason = %reason, "Review escalated to supervisor");
                    let _ = feedback;
                    completed_indices.push((idx, false));
                }
            }
        }

        completed_indices.sort_by(|a, b| b.0.cmp(&a.0));
        completed_indices.dedup();

        for (idx, approved) in completed_indices.into_iter() {
            if let Some(active) = self.active_reviews.get(idx) {
                // Clear context before the active review is removed to avoid stale
                // review/task IDs persisting on reused reviewer panes.
                self.publish_review_context(active, None);
            }
            let review = self.active_reviews.remove(idx);
            self.record_calibration_data(&review);

            if let Err(e) = review.panel.kill_all_sessions().await {
                warn!(review_id = %review.review_id, error = %e, "Failed to terminate review sessions after terminal outcome");
            }

            if approved {
                self.queue
                    .ack(&review.claim_id)
                    .await
                    .map_err(ReviewError::Queue)?;
                self.lifecycle
                    .emit_approval(&review.review_id.to_string())
                    .await?;
                info!(review_id = %review.review_id, rounds = review.round, "Review approved");
            }
        }

        Ok(())
    }

    /// Start a new review.
    async fn start_review(
        &mut self,
        item_id: String,
        claim_id: ClaimId,
    ) -> Result<(), ReviewError> {
        let review_id = ReviewId::new(&item_id);
        let reviewers = self.select_reviewers()?;

        if reviewers.is_empty() {
            self.queue
                .ack(&claim_id)
                .await
                .map_err(ReviewError::Queue)?;
            return Err(ReviewError::Config("No reviewers available".into()));
        }

        let affinity = PanelAffinity::new(review_id.clone(), item_id.clone(), reviewers.clone());

        let mut panel = ReviewPanel::new(
            affinity,
            self.gateway.clone(),
            "worktree".to_string(),
            "branch".to_string(),
        );

        if let Err(e) = panel.spawn_sessions().await {
            if let Err(cleanup_err) = panel.kill_all_sessions().await {
                warn!(
                    review_id = %review_id,
                    error = %cleanup_err,
                    "Failed to terminate review sessions during start_review cleanup"
                );
            }
            return Err(e.into());
        }

        let review = ActiveReview {
            review_id,
            task_id: item_id,
            branch: String::new(),
            base_commit: String::new(),
            claim_id,
            panel,
            round: 1,
            collector: ScoreCollector::new(),
            chunks: Vec::new(),
            current_chunk: 0,
        };

        self.active_reviews.push(review);
        if let Some(active) = self.active_reviews.last() {
            let snapshot = Self::build_review_context_snapshot(active, None, None);
            self.publish_review_context(active, Some(snapshot));
        }

        Ok(())
    }

    /// Select reviewers for a panel.
    fn select_reviewers(&self) -> Result<Vec<String>, ReviewError> {
        if self.config.default_reviewers.is_empty() {
            return Err(ReviewError::Config(
                "No default reviewers configured".into(),
            ));
        }

        let count = self
            .config
            .panel_size
            .min(self.config.default_reviewers.len());
        Ok(self.config.default_reviewers[..count].to_vec())
    }

    /// Evaluate the current round of a review.
    async fn evaluate_round(&self, review: &ActiveReview) -> Result<ReviewOutcome, ReviewError> {
        self.lifecycle
            .process_round(&review.panel, &review.collector, review.round)
            .await
            .map_err(ReviewError::Lifecycle)
    }

    /// Record calibration data for reviewers.
    fn record_calibration_data(&mut self, review: &ActiveReview) {
        for (reviewer_id, submission) in review.panel.submissions() {
            let is_approval = submission.verdict == brehon_types::ReviewVerdict::Approve;
            let is_rejection = submission.verdict == brehon_types::ReviewVerdict::Reject;

            self.calibration.record_review(
                reviewer_id,
                submission.score.as_u8(),
                is_approval,
                is_rejection,
            );
        }
    }

    /// Submit a review score.
    pub fn submit_review(
        &mut self,
        review_id: &str,
        submission: ReviewerSubmission,
    ) -> Result<(), ReviewError> {
        let reviewer_id = submission.reviewer_id.clone();
        let score = submission.score;
        let verdict = submission.verdict;
        {
            let review = self
                .active_reviews
                .iter_mut()
                .find(|r| r.review_id.as_str() == review_id)
                .ok_or_else(|| {
                    ReviewError::Panel(crate::panel::PanelError::NotFound(review_id.to_string()))
                })?;
            review.panel.submit(submission);
            review.collector.add(reviewer_id, score, verdict);
        }

        if let Some(review) = self
            .active_reviews
            .iter()
            .find(|r| r.review_id.as_str() == review_id)
        {
            let snapshot = Self::build_review_context_snapshot(review, None, None);
            self.publish_review_context(review, Some(snapshot));
        }

        Ok(())
    }

    /// Get calibration stats.
    pub fn calibration_stats(&self) -> &ReviewerCalibration {
        &self.calibration
    }

    /// Derive a stability counter snapshot from the review coordinator.
    ///
    /// Only the `active_reviews` counter is populated; other fields are left at
    /// zero for the caller to merge with subsystem-specific snapshots.
    pub fn stability_counters(&self) -> StabilityCounters {
        StabilityCounters {
            active_reviews: self.active_reviews.len(),
            ..StabilityCounters::default()
        }
    }

    /// Check for stale reviews.
    pub async fn check_stale_reviews(&self) -> Result<Vec<String>, ReviewError> {
        let mut stale_reviews = Vec::new();

        for review in &self.active_reviews {
            if let Some(ref detector) = self.stale_detector {
                let result = detector
                    .check(&review.branch, &review.base_commit, "main")
                    .await
                    .map_err(ReviewError::StaleDetection)?;

                if result == StaleDetection::Stale {
                    stale_reviews.push(review.review_id.to_string());
                }
            }
        }

        Ok(stale_reviews)
    }

    fn publish_review_context(
        &self,
        review: &ActiveReview,
        context: Option<ReviewContextSnapshot>,
    ) {
        let Some(sink) = &self.review_context_sink else {
            return;
        };

        let sessions = review.panel.sessions();
        if sessions.is_empty() {
            // Startup/session races can emit updates before reviewer sessions are
            // attached; target reviewer pane IDs directly as a fallback.
            for reviewer_id in &review.panel.affinity().reviewer_ids {
                sink(
                    ReviewContextTarget::PaneId(reviewer_id.clone()),
                    context.clone(),
                );
            }
            return;
        }

        for session_id in sessions.values() {
            sink(
                ReviewContextTarget::SessionId(session_id.as_str().to_string()),
                context.clone(),
            );
        }
    }

    fn build_review_context_snapshot(
        review: &ActiveReview,
        verdict: Option<String>,
        findings_summary: Option<String>,
    ) -> ReviewContextSnapshot {
        let panel_total = review.panel.affinity().reviewer_ids.len();
        let panel_done = review.panel.submissions().len();
        let score = review
            .collector
            .average_score()
            .map(|avg| avg.round().clamp(0.0, 10.0) as u8);
        ReviewContextSnapshot {
            review_id: review.review_id.to_string(),
            task_id: review.task_id.clone(),
            round: review.round,
            panel_total,
            panel_done,
            verdict,
            score,
            findings_summary,
            updated_at: std::time::Instant::now(),
        }
    }

    fn summarize_round_findings_canonical(review: &ActiveReview) -> Option<String> {
        let mut blocking = 0usize;
        let mut suggestions = 0usize;
        let mut nitpicks = 0usize;

        for submission in review.panel.submissions().values() {
            for finding in &submission.findings {
                match finding.severity {
                    CommentSeverity::Blocking => blocking += 1,
                    CommentSeverity::Suggestion => suggestions += 1,
                    CommentSeverity::Nitpick => nitpicks += 1,
                }
            }
        }

        Self::format_findings_summary(blocking, suggestions, nitpicks, 0)
    }

    fn summarize_consolidated_feedback(
        feedback: &brehon_types::ConsolidatedFeedback,
    ) -> Option<String> {
        Self::format_findings_summary(
            feedback.blocking.len(),
            feedback.suggestions.len(),
            feedback.nitpicks.len(),
            feedback.dissent.len(),
        )
    }

    fn format_findings_summary(
        blocking: usize,
        suggestions: usize,
        nitpicks: usize,
        dissent: usize,
    ) -> Option<String> {
        let total = blocking + suggestions + nitpicks + dissent;
        (total > 0).then(|| {
            format!(
                "blocking {} | suggestions {} | nitpicks {} | dissent {}",
                blocking, suggestions, nitpicks, dissent
            )
        })
    }

    fn verdict_label(verdict: ReviewVerdict) -> String {
        match verdict {
            ReviewVerdict::Approve => "approve".to_string(),
            ReviewVerdict::ChangesRequested => "changes_requested".to_string(),
            ReviewVerdict::Reject => "reject".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_test_harness::{FakeGitOperations, InMemoryEventStore, MockGateway};
    use brehon_types::{config::ChunkStrategy, ReviewScore, SessionId};
    use std::sync::Mutex as StdMutex;

    fn make_config() -> CoordinatorConfig {
        CoordinatorConfig {
            panel_size: 3,
            default_reviewers: vec![
                "reviewer-1".to_string(),
                "reviewer-2".to_string(),
                "reviewer-3".to_string(),
            ],
            claim_lease_duration: Duration::from_secs(300),
            poll_interval: Duration::from_secs(5),
            policy: ReviewPolicy::default(),
            chunking: ChunkingConfig::new(8000, ChunkStrategy::ByDirectory),
            stale_detection_enabled: false,
        }
    }

    #[test]
    fn coordinator_config_default() {
        let config = CoordinatorConfig::default();

        assert_eq!(config.panel_size, 3);
        assert!(config.default_reviewers.is_empty());
        assert_eq!(config.policy.min_average_score, 7);
    }

    #[test]
    fn select_reviewers() {
        let config = make_config();
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());
        let git = Arc::new(FakeGitOperations::new());

        let coordinator = ReviewCoordinator::new(config, store, gateway, git);

        let reviewers = coordinator.select_reviewers().unwrap();
        assert_eq!(reviewers.len(), 3);
    }

    #[test]
    fn select_reviewers_none_configured() {
        let config = CoordinatorConfig {
            default_reviewers: vec![],
            ..CoordinatorConfig::default()
        };
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());
        let git = Arc::new(FakeGitOperations::new());

        let coordinator = ReviewCoordinator::new(config, store, gateway, git);

        let result = coordinator.select_reviewers();
        assert!(result.is_err());
    }

    fn make_active_review() -> ActiveReview {
        make_active_review_with_gateway(Arc::new(MockGateway::new()))
    }

    fn make_active_review_with_gateway(gateway: Arc<MockGateway>) -> ActiveReview {
        let affinity = PanelAffinity::new(
            ReviewId::new("R-snapshot"),
            "T-snapshot".to_string(),
            vec![
                "reviewer-1".to_string(),
                "reviewer-2".to_string(),
                "reviewer-3".to_string(),
            ],
        );
        let mut panel = ReviewPanel::new(
            affinity,
            gateway,
            "worktree".to_string(),
            "branch".to_string(),
        );
        panel
            .affinity_mut()
            .sessions
            .insert("reviewer-1".to_string(), SessionId::new("s-1"));
        panel
            .affinity_mut()
            .sessions
            .insert("reviewer-2".to_string(), SessionId::new("s-2"));
        panel
            .affinity_mut()
            .sessions
            .insert("reviewer-3".to_string(), SessionId::new("s-3"));

        let mut collector = ScoreCollector::new();
        collector.add(
            "reviewer-1".to_string(),
            ReviewScore::new(8),
            ReviewVerdict::Approve,
        );

        panel.submit(ReviewerSubmission {
            reviewer_id: "reviewer-1".to_string(),
            session_id: SessionId::new("s-1"),
            score: ReviewScore::new(8),
            verdict: ReviewVerdict::Approve,
            findings: vec![],
        });

        ActiveReview {
            review_id: ReviewId::new("R-snapshot"),
            task_id: "T-snapshot".to_string(),
            branch: "feature/branch".to_string(),
            base_commit: "abc123".to_string(),
            claim_id: ClaimId::new("claim-1"),
            panel,
            round: 1,
            collector,
            chunks: Vec::new(),
            current_chunk: 0,
        }
    }

    #[test]
    fn build_review_context_snapshot_mid_round_without_verdict() {
        let review = make_active_review();

        let snapshot = ReviewCoordinator::build_review_context_snapshot(&review, None, None);

        assert_eq!(snapshot.review_id, "R-snapshot");
        assert_eq!(snapshot.task_id, "T-snapshot");
        assert_eq!(snapshot.round, 1);
        assert_eq!(snapshot.panel_total, 3);
        assert_eq!(snapshot.panel_done, 1);
        assert_eq!(snapshot.verdict, None);
        assert_eq!(snapshot.score, Some(8));
        assert_eq!(snapshot.findings_summary, None);
    }

    #[test]
    fn build_review_context_snapshot_completed_with_verdict_and_summary() {
        let mut review = make_active_review();
        review.collector.add(
            "reviewer-2".to_string(),
            ReviewScore::new(9),
            ReviewVerdict::Approve,
        );
        review.collector.add(
            "reviewer-3".to_string(),
            ReviewScore::new(8),
            ReviewVerdict::Approve,
        );
        review.panel.submit(ReviewerSubmission {
            reviewer_id: "reviewer-2".to_string(),
            session_id: SessionId::new("s-2"),
            score: ReviewScore::new(9),
            verdict: ReviewVerdict::Approve,
            findings: vec![],
        });
        review.panel.submit(ReviewerSubmission {
            reviewer_id: "reviewer-3".to_string(),
            session_id: SessionId::new("s-3"),
            score: ReviewScore::new(8),
            verdict: ReviewVerdict::Approve,
            findings: vec![],
        });

        let summary = Some("blocking 0 | suggestions 1 | nitpicks 0 | dissent 0".to_string());
        let snapshot = ReviewCoordinator::build_review_context_snapshot(
            &review,
            Some("approve".to_string()),
            summary.clone(),
        );

        assert_eq!(snapshot.panel_done, 3);
        assert_eq!(snapshot.verdict, Some("approve".to_string()));
        assert_eq!(snapshot.score, Some(8));
        assert_eq!(snapshot.findings_summary, summary);
    }

    #[tokio::test]
    async fn clears_review_context_when_terminal_review_is_removed() {
        let config = make_config();
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());
        let git = Arc::new(FakeGitOperations::new());

        let events: Arc<StdMutex<Vec<(String, bool)>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink_events = events.clone();
        let sink: ReviewContextSink = Arc::new(move |target, context| {
            let key = match target {
                ReviewContextTarget::PaneId(pane_id) => pane_id,
                ReviewContextTarget::SessionId(session_id) => session_id,
            };
            sink_events
                .lock()
                .expect("lock sink events")
                .push((key, context.is_none()));
        });

        let mut coordinator = ReviewCoordinator::new(config, store, gateway, git)
            .with_review_context_sink_for_test(sink);

        coordinator
            .start_review("T-terminal".to_string(), ClaimId::new("claim-terminal"))
            .await
            .expect("start review");

        let review_id = coordinator.active_reviews[0].review_id.to_string();
        for (reviewer, score, verdict) in [
            ("reviewer-1", 8, ReviewVerdict::Approve),
            ("reviewer-2", 8, ReviewVerdict::Approve),
            ("reviewer-3", 3, ReviewVerdict::Reject),
        ] {
            coordinator
                .submit_review(
                    &review_id,
                    ReviewerSubmission {
                        reviewer_id: reviewer.to_string(),
                        session_id: SessionId::new(format!("s-{reviewer}")),
                        score: ReviewScore::new(score),
                        verdict,
                        findings: vec![],
                    },
                )
                .expect("submit review");
        }

        coordinator
            .check_active_reviews()
            .await
            .expect("check reviews");
        assert!(
            coordinator.active_reviews.is_empty(),
            "terminal review should be removed from active set"
        );

        let events = events.lock().expect("lock events");
        let cleared: std::collections::HashSet<String> = events
            .iter()
            .filter_map(|(target, is_clear)| (*is_clear).then_some(target.clone()))
            .collect();
        assert!(
            cleared.len() == 3,
            "all reviewer targets should receive a clear context update"
        );
    }

    #[tokio::test]
    async fn shutdown_tears_down_active_sessions() {
        let config = make_config();
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());
        let git = Arc::new(FakeGitOperations::new());

        let mut coordinator = ReviewCoordinator::new(config, store, gateway.clone(), git);

        coordinator
            .start_review("T-shutdown".to_string(), ClaimId::new("claim-shutdown"))
            .await
            .expect("start review");

        coordinator
            .shutdown()
            .await
            .expect("shutdown should terminate sessions");

        assert_eq!(
            coordinator.active_reviews.len(),
            0,
            "shutdown should clear active reviews"
        );

        let kill_calls = gateway
            .calls()
            .into_iter()
            .filter(|call| call.method == "kill_session")
            .count();

        assert_eq!(
            kill_calls, 3,
            "shutdown should terminate all reviewer sessions exactly once"
        );
    }

    #[tokio::test]
    async fn start_review_cleans_up_partial_sessions_on_spawn_failure() {
        let config = make_config();
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());
        let git = Arc::new(FakeGitOperations::new());

        gateway.set_crash_after_create(1);

        let mut coordinator = ReviewCoordinator::new(config, store, gateway.clone(), git);

        let start_err = coordinator
            .start_review("T-start-fail".to_string(), ClaimId::new("claim-start-fail"))
            .await
            .expect_err("start_review should fail after first spawn");

        assert!(matches!(
            start_err,
            ReviewError::Panel(crate::panel::PanelError::SpawnFailed(_))
        ));
        assert!(
            coordinator.active_reviews.is_empty(),
            "failed startup should not keep active reviews"
        );

        let calls = gateway.calls();
        let spawn_calls = calls.iter().filter(|call| call.method == "spawn").count();
        let kill_calls = calls
            .iter()
            .filter(|call| call.method == "kill_session")
            .count();

        assert_eq!(
            spawn_calls, 2,
            "start should attempt all configured spawn attempts"
        );
        assert_eq!(
            kill_calls, 1,
            "partial startup failures should clean up already spawned sessions"
        );
    }

    fn seed_rounded_review(
        review_id: &str,
        round: u32,
        score: u8,
        verdict: ReviewVerdict,
        review: &mut ActiveReview,
    ) {
        review.review_id = ReviewId::new(review_id);
        review.round = round;
        review.collector.clear();

        let reviewer_ids: Vec<String> = review.panel.affinity().reviewer_ids.clone();
        for (i, reviewer_id) in reviewer_ids.iter().enumerate() {
            review
                .collector
                .add(reviewer_id.clone(), ReviewScore::new(score), verdict);
            review.panel.submit(ReviewerSubmission {
                reviewer_id: reviewer_id.clone(),
                session_id: SessionId::new(format!("session-{}-{}", i, reviewer_id)),
                score: ReviewScore::new(score),
                verdict,
                findings: vec![],
            });
        }
    }

    #[tokio::test]
    async fn terminal_review_outcomes_kill_all_active_sessions() {
        let config = make_config();
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());
        let git = Arc::new(FakeGitOperations::new());

        let mut coordinator = ReviewCoordinator::new(config, store, gateway.clone(), git);

        let mut rejected = make_active_review_with_gateway(gateway.clone());
        seed_rounded_review("R-rejected", 1, 3, ReviewVerdict::Reject, &mut rejected);

        let mut exhausted = make_active_review_with_gateway(gateway.clone());
        seed_rounded_review("R-exhausted", 3, 4, ReviewVerdict::Approve, &mut exhausted);

        coordinator.active_reviews.push(rejected);
        coordinator.active_reviews.push(exhausted);

        coordinator
            .check_active_reviews()
            .await
            .expect("check terminal outcomes");

        assert_eq!(
            coordinator.active_reviews.len(),
            0,
            "terminal outcomes should clear all active reviews"
        );

        let kill_calls = gateway
            .calls()
            .into_iter()
            .filter(|call| call.method == "kill_session")
            .count();

        assert_eq!(
            kill_calls, 6,
            "terminal review outcomes should tear down all sessions once each"
        );
    }

    #[test]
    fn stability_counters_empty() {
        let config = make_config();
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());
        let git = Arc::new(FakeGitOperations::new());

        let coordinator = ReviewCoordinator::new(config, store, gateway, git);
        let counters = coordinator.stability_counters();

        assert_eq!(counters.active_reviews, 0);
        assert_eq!(counters.pending_requests, 0);
        assert_eq!(counters.pending_prompt_waiters, 0);
        assert_eq!(counters.blocked_sends, 0);
    }
}

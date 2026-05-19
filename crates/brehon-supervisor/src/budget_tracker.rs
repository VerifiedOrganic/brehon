//! Token budget tracking and enforcement.
//!
//! Tracks TokenUsage from ResponseReceived events and emits notifications
//! when limits are reached.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::RwLock;
use tracing::{debug, warn};

use brehon_ports::NotificationSink;
use brehon_types::{BudgetState, Event, EventKind, TokenUsage};

#[derive(Debug, Clone)]
pub struct BudgetPolicy {
    pub max_total_tokens: Option<u64>,
    pub max_total_cost: Option<f64>,
    pub soft_threshold_percent: u8,
    pub hard_limit: bool,
}

impl Default for BudgetPolicy {
    fn default() -> Self {
        Self {
            max_total_tokens: None,
            max_total_cost: None,
            soft_threshold_percent: 80,
            hard_limit: true,
        }
    }
}

impl BudgetPolicy {
    pub fn new(max_total_tokens: u64) -> Self {
        Self {
            max_total_tokens: Some(max_total_tokens),
            ..Self::default()
        }
    }

    pub fn with_cost_limit(mut self, max_cost: f64) -> Self {
        self.max_total_cost = Some(max_cost);
        self
    }

    pub fn soft_threshold_tokens(&self) -> Option<u64> {
        self.max_total_tokens
            .map(|max| (max as f64 * self.soft_threshold_percent as f64 / 100.0) as u64)
    }

    pub fn is_soft_threshold_reached(&self, total_tokens: u64) -> bool {
        if let Some(threshold) = self.soft_threshold_tokens() {
            total_tokens >= threshold
        } else {
            false
        }
    }

    pub fn is_hard_limit_reached(&self, total_tokens: u64) -> bool {
        if let Some(max) = self.max_total_tokens {
            total_tokens >= max
        } else {
            false
        }
    }
}

pub struct BudgetTracker {
    state: RwLock<BudgetState>,
    policy: BudgetPolicy,
    notifications: Option<Arc<dyn NotificationSink>>,
    soft_threshold_triggered: RwLock<bool>,
    max_per_agent_records: usize,
}

impl BudgetTracker {
    pub fn new(policy: BudgetPolicy) -> Self {
        Self {
            state: RwLock::new(BudgetState::default()),
            policy,
            notifications: None,
            soft_threshold_triggered: RwLock::new(false),
            max_per_agent_records: 1_000,
        }
    }

    pub fn with_notifications(mut self, notifications: Arc<dyn NotificationSink>) -> Self {
        self.notifications = Some(notifications);
        self
    }

    pub fn set_max_per_agent_records(&mut self, max: usize) {
        self.max_per_agent_records = max;
    }

    pub fn state(&self) -> BudgetState {
        self.state.read().clone()
    }

    pub fn total_tokens(&self) -> u64 {
        self.state.read().total_tokens
    }

    pub fn total_cost(&self) -> f64 {
        self.state.read().total_cost
    }

    pub fn tokens_by_agent(&self) -> HashMap<String, u64> {
        let mut result = HashMap::new();
        for (k, v) in self.state.read().tokens_by_agent.iter() {
            *result.entry(k.clone()).or_insert(0) += v;
        }
        result
    }

    pub fn record_usage(&self, usage: TokenUsage) {
        let cost = usage.model.as_ref().map(|m| estimate_cost(m, &usage));

        {
            let mut state = self.state.write();

            state.total_tokens += usage.total_tokens;
            state
                .tokens_by_agent
                .push((usage.agent_id.clone(), usage.total_tokens));

            state.updated_at = Utc::now();

            if let Some(c) = cost {
                state.total_cost += c;
                state.cost_by_agent.push((usage.agent_id.clone(), c));
            }

            if state.tokens_by_agent.len() > self.max_per_agent_records {
                let excess = state.tokens_by_agent.len() - self.max_per_agent_records;
                state.tokens_by_agent.drain(0..excess);
            }
            if state.cost_by_agent.len() > self.max_per_agent_records {
                let excess = state.cost_by_agent.len() - self.max_per_agent_records;
                state.cost_by_agent.drain(0..excess);
            }
        }

        let total = self.total_tokens();
        let soft_reached = self.policy.is_soft_threshold_reached(total);
        let hard_reached = self.policy.is_hard_limit_reached(total);

        if soft_reached && !*self.soft_threshold_triggered.read() {
            *self.soft_threshold_triggered.write() = true;
            self.emit_soft_threshold_notification(usage.total_tokens);
        }

        if hard_reached {
            self.emit_hard_limit_event(usage.total_tokens);
        }
    }

    pub fn process_event(&self, event: &Event) {
        if let EventKind::ResponseReceived {
            session_id,
            tokens_used,
            ..
        } = &event.kind
        {
            let usage = TokenUsage::new(*tokens_used, 0, session_id.clone());
            self.record_usage(usage);

            debug!(
                session_id = %session_id,
                tokens = tokens_used,
                total = %self.total_tokens(),
                "Recorded token usage"
            );
        }
    }

    fn emit_soft_threshold_notification(&self, current_tokens: u64) {
        if let Some(ref notifications) = self.notifications {
            let message = format!(
                "Budget at {}% threshold ({} tokens used)",
                self.policy.soft_threshold_percent, current_tokens
            );
            debug!("{}", message);

            let notification = brehon_ports::Notification::warning(message);
            let _ = notifications.toast(notification);
        }
    }

    fn emit_hard_limit_event(&self, current_tokens: u64) {
        warn!(
            current_tokens = current_tokens,
            max_tokens = ?self.policy.max_total_tokens,
            "Hard budget limit reached"
        );
    }

    pub fn reset(&self) {
        *self.state.write() = BudgetState::default();
        *self.soft_threshold_triggered.write() = false;
    }

    pub fn is_over_budget(&self) -> bool {
        self.policy.is_hard_limit_reached(self.total_tokens())
    }

    pub fn percent_used(&self) -> f64 {
        if let Some(max) = self.policy.max_total_tokens {
            (self.total_tokens() as f64 / max as f64) * 100.0
        } else {
            0.0
        }
    }
}

fn estimate_cost(model: &str, usage: &TokenUsage) -> f64 {
    let cost_per_million_input: f64 = match model {
        m if m.contains("claude-3-opus") => 15.0,
        m if m.contains("claude-3-sonnet") => 3.0,
        m if m.contains("claude-3-haiku") => 0.25,
        m if m.contains("gpt-4-turbo") => 10.0,
        m if m.contains("gpt-4") => 30.0,
        m if m.contains("gpt-3.5") => 0.5,
        _ => 1.0,
    };

    let cost_per_million_output = cost_per_million_input * 3.0;

    let input_cost = (usage.input_tokens as f64 / 1_000_000.0) * cost_per_million_input;
    let output_cost = (usage.output_tokens as f64 / 1_000_000.0) * cost_per_million_output;

    input_cost + output_cost
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_test_harness::RecordingNotificationSink;

    #[test]
    fn budget_policy_soft_threshold() {
        let policy = BudgetPolicy::new(1000);
        assert_eq!(policy.soft_threshold_tokens(), Some(800));
        assert!(policy.is_soft_threshold_reached(800));
        assert!(!policy.is_soft_threshold_reached(799));
    }

    #[test]
    fn budget_policy_hard_limit() {
        let policy = BudgetPolicy::new(1000);
        assert!(policy.is_hard_limit_reached(1000));
        assert!(policy.is_hard_limit_reached(1001));
        assert!(!policy.is_hard_limit_reached(999));
    }

    #[test]
    fn budget_tracker_record_usage() {
        let tracker = BudgetTracker::new(BudgetPolicy::new(10000));
        tracker.record_usage(TokenUsage::new(100, 50, "agent-1"));
        tracker.record_usage(TokenUsage::new(200, 100, "agent-2"));

        assert_eq!(tracker.total_tokens(), 450);
    }

    #[test]
    fn budget_tracker_soft_threshold_notification() {
        let notifications = Arc::new(RecordingNotificationSink::new());
        let policy = BudgetPolicy {
            max_total_tokens: Some(1000),
            soft_threshold_percent: 80,
            ..Default::default()
        };
        let tracker = BudgetTracker::new(policy).with_notifications(notifications.clone());

        for i in 0..8 {
            tracker.record_usage(TokenUsage::new(100, 0, format!("agent-{}", i)));
        }

        assert_eq!(tracker.total_tokens(), 800);
        assert!(*tracker.soft_threshold_triggered.read());
        assert!(notifications.was_toast_called());
    }

    #[test]
    fn budget_tracker_hard_limit() {
        let policy = BudgetPolicy {
            max_total_tokens: Some(1000),
            hard_limit: true,
            ..Default::default()
        };
        let tracker = BudgetTracker::new(policy);

        tracker.record_usage(TokenUsage::new(500, 500, "agent-1"));
        assert!(tracker.is_over_budget());
    }

    #[test]
    fn budget_tracker_percent_used() {
        let tracker = BudgetTracker::new(BudgetPolicy::new(1000));
        tracker.record_usage(TokenUsage::new(250, 0, "agent-1"));
        assert_eq!(tracker.percent_used(), 25.0);
    }

    #[test]
    fn budget_tracker_by_agent() {
        let tracker = BudgetTracker::new(BudgetPolicy::new(10000));
        tracker.record_usage(TokenUsage::new(100, 50, "agent-1"));
        tracker.record_usage(TokenUsage::new(200, 100, "agent-2"));
        tracker.record_usage(TokenUsage::new(50, 25, "agent-1"));

        let by_agent = tracker.tokens_by_agent();
        assert_eq!(by_agent.get("agent-1"), Some(&225));
        assert_eq!(by_agent.get("agent-2"), Some(&300));
    }

    #[test]
    fn budget_tracker_reset() {
        let tracker = BudgetTracker::new(BudgetPolicy::new(1000));
        tracker.record_usage(TokenUsage::new(100, 50, "agent-1"));
        assert_eq!(tracker.total_tokens(), 150);

        tracker.reset();
        assert_eq!(tracker.total_tokens(), 0);
    }

    #[test]
    fn estimate_cost_for_models() {
        let usage = TokenUsage::new(1_000_000, 500_000, "test").with_model("claude-3-opus");
        let cost = estimate_cost("claude-3-opus", &usage);
        assert!(cost > 0.0);
        assert!(cost < 100.0);
    }

    #[test]
    fn per_agent_records_are_bounded() {
        let mut tracker = BudgetTracker::new(BudgetPolicy::new(10000));
        tracker.set_max_per_agent_records(5);
        for i in 0..10 {
            tracker.record_usage(
                TokenUsage::new(10, 0, format!("agent-{}", i % 2)).with_model("claude-3-opus"),
            );
        }
        let state = tracker.state();
        assert_eq!(state.tokens_by_agent.len(), 5);
        assert_eq!(state.cost_by_agent.len(), 5);
    }
}

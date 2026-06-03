//! Periodic AI sanity checks (heartbeat) for guided mode.
//!
//! Constructs compact state summary for AI and invokes DecisionEngine periodically.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tracing::debug;

use brehon_ports::{DecisionEngine, PortError};
use brehon_types::{DecisionConfidence, DecisionKind, DecisionRequest, DecisionResponse};

#[derive(Debug, Clone)]
pub struct HeartbeatConfig {
    pub interval_minutes: u32,
    pub enabled: bool,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval_minutes: 5,
            enabled: true,
        }
    }
}

impl HeartbeatConfig {
    pub fn new(interval_minutes: u32) -> Self {
        Self {
            interval_minutes,
            enabled: true,
        }
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs(u64::from(self.interval_minutes) * 60)
    }
}

#[derive(Debug, Clone)]
pub struct HeartbeatState {
    pub last_heartbeat: Option<DateTime<Utc>>,
    pub last_confidence: Option<DecisionConfidence>,
    pub consecutive_low_confidence: u32,
    pub total_checkouts: u32,
}

impl Default for HeartbeatState {
    fn default() -> Self {
        Self::new()
    }
}

impl HeartbeatState {
    pub fn new() -> Self {
        Self {
            last_heartbeat: None,
            last_confidence: None,
            consecutive_low_confidence: 0,
            total_checkouts: 0,
        }
    }

    pub fn should_run_heartbeat(&self, config: &HeartbeatConfig) -> bool {
        if !config.enabled {
            return false;
        }

        match self.last_heartbeat {
            None => true,
            Some(last) => {
                let elapsed = Utc::now() - last;
                elapsed
                    >= chrono::Duration::from_std(config.interval()).unwrap_or_else(|_| {
                        chrono::Duration::seconds(i64::from(config.interval_minutes) * 60)
                    })
            }
        }
    }

    pub fn record_heartbeat(&mut self, confidence: DecisionConfidence) {
        self.last_heartbeat = Some(Utc::now());
        self.last_confidence = Some(confidence);
        self.total_checkouts += 1;

        if confidence == DecisionConfidence::Low {
            self.consecutive_low_confidence += 1;
        } else {
            self.consecutive_low_confidence = 0;
        }
    }

    #[allow(dead_code)]
    pub fn needs_escalation(&self, threshold: u32) -> bool {
        self.consecutive_low_confidence >= threshold
    }
}

pub struct HeartbeatSummary {
    pub active_agents: usize,
    pub active_tasks: usize,
    pub pending_tasks: usize,
    pub stuck_agents: usize,
    pub recent_events: usize,
    pub budget_used_percent: f64,
    pub system_state: String,
}

pub struct HeartbeatRunner {
    config: HeartbeatConfig,
    state: HeartbeatState,
    decision_engine: Arc<dyn DecisionEngine>,
}

impl HeartbeatRunner {
    pub fn new(config: HeartbeatConfig, decision_engine: Arc<dyn DecisionEngine>) -> Self {
        Self {
            config,
            state: HeartbeatState::new(),
            decision_engine,
        }
    }

    #[allow(dead_code)]
    pub fn state(&self) -> &HeartbeatState {
        &self.state
    }

    #[allow(dead_code)]
    pub fn state_mut(&mut self) -> &mut HeartbeatState {
        &mut self.state
    }

    pub fn should_run(&self) -> bool {
        self.state.should_run_heartbeat(&self.config)
    }

    pub async fn run_heartbeat(
        &mut self,
        summary: HeartbeatSummary,
    ) -> Result<DecisionResponse, PortError> {
        debug!("Running heartbeat check");

        let context = format!(
            "Active agents: {}\nActive tasks: {}\nPending tasks: {}\nStuck agents: {}\nRecent events: {}\nBudget used: {:.1}%\nSystem state: {}",
            summary.active_agents,
            summary.active_tasks,
            summary.pending_tasks,
            summary.stuck_agents,
            summary.recent_events,
            summary.budget_used_percent,
            summary.system_state
        );

        let request = DecisionRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            kind: DecisionKind::HeartbeatCheck,
            context,
            event_ids: vec![],
            options: vec!["proceed".into(), "stuck".into(), "escalate".into()],
            created_at: Utc::now(),
        };

        let response = self.decision_engine.decide(request).await?;

        debug!(
            confidence = ?response.confidence,
            decision = %response.decision,
            "Heartbeat completed"
        );

        self.state.record_heartbeat(response.confidence);

        Ok(response)
    }

    #[allow(dead_code)]
    pub fn needs_escalation(&self, threshold: u32) -> bool {
        self.state.needs_escalation(threshold)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_test_harness::MockDecisionEngine;

    #[test]
    fn heartbeat_config_interval() {
        let config = HeartbeatConfig::new(10);
        assert_eq!(config.interval_minutes, 10);
        assert_eq!(config.interval(), Duration::from_secs(10 * 60));
    }

    #[test]
    fn heartbeat_state_initial() {
        let state = HeartbeatState::new();
        assert!(state.last_heartbeat.is_none());
        assert!(state.last_confidence.is_none());
        assert_eq!(state.consecutive_low_confidence, 0);
        assert_eq!(state.total_checkouts, 0);
    }

    #[test]
    fn heartbeat_state_record() {
        let mut state = HeartbeatState::new();

        state.record_heartbeat(DecisionConfidence::High);
        assert!(state.last_heartbeat.is_some());
        assert_eq!(state.last_confidence, Some(DecisionConfidence::High));
        assert_eq!(state.total_checkouts, 1);
        assert_eq!(state.consecutive_low_confidence, 0);

        state.record_heartbeat(DecisionConfidence::Low);
        assert_eq!(state.consecutive_low_confidence, 1);

        state.record_heartbeat(DecisionConfidence::Medium);
        assert_eq!(state.consecutive_low_confidence, 0);
    }

    #[test]
    fn heartbeat_should_run() {
        let config = HeartbeatConfig::new(5);
        let mut state = HeartbeatState::new();

        assert!(state.should_run_heartbeat(&config));

        state.record_heartbeat(DecisionConfidence::High);
        assert!(!state.should_run_heartbeat(&config));
    }

    #[test]
    fn needs_escalation() {
        let mut state = HeartbeatState::new();

        assert!(!state.needs_escalation(3));

        state.record_heartbeat(DecisionConfidence::Low);
        assert!(!state.needs_escalation(3));

        state.record_heartbeat(DecisionConfidence::Low);
        state.record_heartbeat(DecisionConfidence::Low);
        assert!(state.needs_escalation(3));
    }

    #[tokio::test]
    async fn run_heartbeat() {
        let decision_engine = Arc::new(MockDecisionEngine::new());
        let mut runner = HeartbeatRunner::new(HeartbeatConfig::default(), decision_engine);

        let summary = HeartbeatSummary {
            active_agents: 3,
            active_tasks: 2,
            pending_tasks: 5,
            stuck_agents: 0,
            recent_events: 20,
            budget_used_percent: 35.0,
            system_state: "running".into(),
        };

        let _response = runner.run_heartbeat(summary).await.unwrap();
        assert_eq!(runner.state().total_checkouts, 1);
        assert!(runner.state().last_heartbeat.is_some());
    }
}

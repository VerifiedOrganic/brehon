//! Stuck detection for the supervisor.
//!
//! Detects stuck workers and agents based on time and pattern analysis.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tracing::debug;

use crate::event_monitor::AgentState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecommendedAction {
    NudgeWorker,
    RequestReview,
    Reassign,
    CheckReview,
    Escalate,
}

#[derive(Debug, Clone)]
pub struct StuckDetectionConfig {
    pub time_threshold_minutes: u64,
    pub pattern_threshold: usize,
    pub pattern_window_minutes: u32,
    pub operation_aware: bool,
}

impl Default for StuckDetectionConfig {
    fn default() -> Self {
        Self {
            time_threshold_minutes: 10,
            pattern_threshold: 3,
            pattern_window_minutes: 5,
            operation_aware: true,
        }
    }
}

impl StuckDetectionConfig {
    pub fn new(time_threshold_minutes: u64) -> Self {
        Self {
            time_threshold_minutes,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct StuckInfo {
    pub session_id: String,
    pub agent_id: String,
    pub idle_minutes: u64,
    pub pattern: Option<String>,
    pub is_in_operation: bool,
    pub task_id: Option<String>,
    pub task_status: Option<String>,
    pub recommended_action: Option<RecommendedAction>,
}

#[derive(Debug, Clone)]
pub struct TaskContext {
    pub task_id: String,
    pub status: String,
    pub assignee_agent_id: Option<String>,
    pub has_active_review: bool,
    pub updated_at: DateTime<Utc>,
}

pub struct StuckDetector {
    config: StuckDetectionConfig,
}

impl StuckDetector {
    pub fn new(config: StuckDetectionConfig) -> Self {
        Self { config }
    }

    pub fn detect_stuck(&self, agents: &[&AgentState]) -> Vec<StuckInfo> {
        let now = Utc::now();
        let mut stuck_agents = Vec::new();

        for agent in agents {
            if !agent.is_alive {
                continue;
            }

            let idle_duration = agent.idle_duration(now);
            let idle_minutes = idle_duration.as_secs() / 60;

            let is_time_stuck = idle_minutes >= self.config.time_threshold_minutes
                && !self.is_exempt_from_time_check(agent);

            let pattern = self.detect_pattern(agent);
            let is_pattern_stuck = pattern.is_some();

            if is_time_stuck || is_pattern_stuck {
                stuck_agents.push(StuckInfo {
                    session_id: agent.session_id.clone(),
                    agent_id: agent.agent_id.clone(),
                    idle_minutes,
                    pattern,
                    is_in_operation: agent.is_in_operation(),
                    task_id: None,
                    task_status: None,
                    recommended_action: None,
                });
            }
        }

        stuck_agents
    }

    fn is_exempt_from_time_check(&self, agent: &AgentState) -> bool {
        if !self.config.operation_aware {
            return false;
        }

        agent.is_in_operation()
    }

    fn detect_pattern(&self, agent: &AgentState) -> Option<String> {
        if self.config.pattern_threshold < 2 {
            return None;
        }

        let recent = agent.recent_messages(self.config.pattern_window_minutes);
        if recent.len() < self.config.pattern_threshold {
            return None;
        }

        let message_counts: HashMap<&str, usize> =
            recent.iter().fold(HashMap::new(), |mut acc, msg| {
                *acc.entry(*msg).or_insert(0) += 1;
                acc
            });

        for (msg, count) in message_counts {
            if count >= self.config.pattern_threshold {
                let pattern = self.classify_pattern(msg);
                debug!(
                    session_id = %agent.session_id,
                    count = count,
                    pattern = %pattern,
                    "Detected stuck pattern"
                );
                return Some(pattern);
            }
        }

        None
    }

    fn classify_pattern(&self, message: &str) -> String {
        let lower = message.to_lowercase();

        if lower.contains("error") || lower.contains("failed") {
            "error_loop".to_string()
        } else if lower.contains("retrying") || lower.contains("trying again") {
            "retry_loop".to_string()
        } else if lower.contains("waiting") || lower.contains("pending") {
            "waiting_loop".to_string()
        } else if lower.contains("timeout") {
            "timeout_loop".to_string()
        } else {
            format!(
                "repeated_message:{}",
                message.chars().take(50).collect::<String>()
            )
        }
    }

    pub fn time_threshold(&self) -> Duration {
        Duration::from_secs(self.config.time_threshold_minutes * 60)
    }
}

pub struct TaskAwareStuckDetector {
    detector: StuckDetector,
}

impl TaskAwareStuckDetector {
    pub fn new(detector: StuckDetector) -> Self {
        Self { detector }
    }

    pub fn detect_stuck_with_tasks(
        &self,
        agents: &[&AgentState],
        tasks: &[TaskContext],
    ) -> Vec<StuckInfo> {
        let mut stuck_agents = self.detector.detect_stuck(agents);

        let task_map: HashMap<&str, &TaskContext> = tasks
            .iter()
            .filter_map(|t| t.assignee_agent_id.as_ref().map(|id| (id.as_str(), t)))
            .collect();

        for stuck in &mut stuck_agents {
            let task = task_map
                .get(stuck.agent_id.as_str())
                .or_else(|| task_map.get(stuck.session_id.as_str()))
                .copied();

            if let Some(task) = task {
                stuck.task_id = Some(task.task_id.clone());
                stuck.task_status = Some(task.status.clone());
                stuck.recommended_action = self.determine_action(
                    &task.status,
                    stuck.idle_minutes,
                    stuck.is_in_operation,
                    task.has_active_review,
                );
            }
        }

        stuck_agents
    }

    fn determine_action(
        &self,
        task_status: &str,
        idle_minutes: u64,
        is_in_operation: bool,
        has_active_review: bool,
    ) -> Option<RecommendedAction> {
        let status_lower = task_status.to_lowercase();
        let normalized = status_lower.replace("_", "");
        match normalized.as_str() {
            "inprogress" => {
                if is_in_operation {
                    None
                } else {
                    Some(RecommendedAction::NudgeWorker)
                }
            }
            "inreview" => {
                if has_active_review {
                    Some(RecommendedAction::CheckReview)
                } else {
                    Some(RecommendedAction::RequestReview)
                }
            }
            "approved" => {
                if idle_minutes >= self.detector.config.time_threshold_minutes {
                    Some(RecommendedAction::Escalate)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub fn reconcile_orphaned_tasks(
        &self,
        agents: &[&AgentState],
        tasks: &[TaskContext],
    ) -> Vec<StuckInfo> {
        let mut orphans = Vec::new();
        let alive_agent_ids: std::collections::HashSet<&str> = agents
            .iter()
            .filter(|a| a.is_alive)
            .map(|a| a.agent_id.as_str())
            .collect();
        let alive_session_ids: std::collections::HashSet<&str> = agents
            .iter()
            .filter(|a| a.is_alive)
            .map(|a| a.session_id.as_str())
            .collect();

        for task in tasks {
            let is_orphaned = task
                .assignee_agent_id
                .as_ref()
                .map(|id| {
                    !alive_agent_ids.contains(id.as_str())
                        && !alive_session_ids.contains(id.as_str())
                })
                .unwrap_or(false);

            if is_orphaned {
                let status_lower = task.status.to_lowercase().replace("_", "");
                let action = match status_lower.as_str() {
                    "inprogress" | "inreview" => Some(RecommendedAction::Reassign),
                    "approved" => Some(RecommendedAction::Escalate),
                    _ => None,
                };

                if let Some(recommended_action) = action {
                    let idle_minutes = (Utc::now() - task.updated_at).num_minutes().max(0) as u64;

                    orphans.push(StuckInfo {
                        session_id: task.assignee_agent_id.clone().unwrap_or_default(),
                        agent_id: task.assignee_agent_id.clone().unwrap_or_default(),
                        idle_minutes,
                        pattern: None,
                        is_in_operation: false,
                        task_id: Some(task.task_id.clone()),
                        task_status: Some(task.status.clone()),
                        recommended_action: Some(recommended_action),
                    });
                }
            }
        }

        orphans
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_agent(session_id: &str, agent_id: &str) -> AgentState {
        AgentState::new(
            agent_id.to_string(),
            session_id.to_string(),
            "worker".to_string(),
        )
    }

    #[test]
    fn detect_time_based_stuck() {
        let config = StuckDetectionConfig::new(5);
        let detector = StuckDetector::new(config);

        let mut agent = make_agent("session-1", "agent-1");
        agent.last_event_at = Utc::now() - chrono::Duration::minutes(10);

        let stuck = detector.detect_stuck(&[&agent]);
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0].session_id, "session-1");
        assert!(stuck[0].idle_minutes >= 10);
    }

    #[test]
    fn no_stuck_when_recently_active() {
        let config = StuckDetectionConfig::new(5);
        let detector = StuckDetector::new(config);

        let agent = make_agent("session-1", "agent-1");

        let stuck = detector.detect_stuck(&[&agent]);
        assert!(stuck.is_empty());
    }

    #[test]
    fn no_stuck_during_operation() {
        let mut config = StuckDetectionConfig::new(5);
        config.operation_aware = true;
        let detector = StuckDetector::new(config);

        let mut agent = make_agent("session-1", "agent-1");
        agent.last_event_at = Utc::now() - chrono::Duration::minutes(10);
        agent.current_operation = Some("cargo test".to_string());

        let stuck = detector.detect_stuck(&[&agent]);
        assert!(stuck.is_empty(), "Agent in operation should not be stuck");
    }

    #[test]
    fn detect_pattern_based_stuck() {
        let config = StuckDetectionConfig {
            pattern_threshold: 3,
            pattern_window_minutes: 5,
            ..Default::default()
        };
        let detector = StuckDetector::new(config);

        let mut agent = make_agent("session-1", "agent-1");
        agent.record_message("error: connection failed".to_string());
        agent.record_message("error: connection failed".to_string());
        agent.record_message("error: connection failed".to_string());

        let stuck = detector.detect_stuck(&[&agent]);
        assert_eq!(stuck.len(), 1);
        assert!(stuck[0].pattern.is_some());
        assert!(stuck[0].pattern.as_ref().unwrap().contains("error"));
    }

    #[test]
    fn no_pattern_with_different_messages() {
        let config = StuckDetectionConfig {
            pattern_threshold: 3,
            pattern_window_minutes: 5,
            ..Default::default()
        };
        let detector = StuckDetector::new(config);

        let mut agent = make_agent("session-1", "agent-1");
        agent.record_message("message 1".to_string());
        agent.record_message("message 2".to_string());
        agent.record_message("message 3".to_string());

        let stuck = detector.detect_stuck(&[&agent]);
        assert!(
            stuck.is_empty(),
            "Different messages should not trigger pattern"
        );
    }

    #[test]
    fn detect_dead_agents_excluded() {
        let config = StuckDetectionConfig::new(5);
        let detector = StuckDetector::new(config);

        let mut agent = make_agent("session-1", "agent-1");
        agent.last_event_at = Utc::now() - chrono::Duration::minutes(10);
        agent.is_alive = false;

        let stuck = detector.detect_stuck(&[&agent]);
        assert!(
            stuck.is_empty(),
            "Dead agents should not be stuck candidates"
        );
    }

    #[test]
    fn operation_aware_config() {
        let mut config = StuckDetectionConfig::new(5);
        config.operation_aware = false;
        let detector = StuckDetector::new(config);

        let mut agent = make_agent("session-1", "agent-1");
        agent.last_event_at = Utc::now() - chrono::Duration::minutes(10);
        agent.current_operation = Some("cargo test".to_string());

        let stuck = detector.detect_stuck(&[&agent]);
        assert_eq!(
            stuck.len(),
            1,
            "Should detect stuck when operation_aware is false"
        );
    }

    #[test]
    fn classify_patterns() {
        let config = StuckDetectionConfig::default();
        let detector = StuckDetector::new(config);

        assert!(detector
            .classify_pattern("Error: failed to connect")
            .contains("error"));
        assert!(detector
            .classify_pattern("Retrying operation...")
            .contains("retry"));
        assert!(detector
            .classify_pattern("Waiting for response")
            .contains("waiting"));
        assert!(detector
            .classify_pattern("Timeout reached")
            .contains("timeout"));
    }

    #[test]
    fn task_aware_detect_stuck_nudge_worker() {
        let config = StuckDetectionConfig::new(5);
        let detector = TaskAwareStuckDetector::new(StuckDetector::new(config));

        let mut agent = make_agent("session-1", "agent-1");
        agent.last_event_at = Utc::now() - chrono::Duration::minutes(10);

        let task = TaskContext {
            task_id: "T001".to_string(),
            status: "in_progress".to_string(),
            assignee_agent_id: Some("agent-1".to_string()),
            has_active_review: false,
            updated_at: Utc::now(),
        };

        let stuck = detector.detect_stuck_with_tasks(&[&agent], &[task]);
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0].task_id, Some("T001".to_string()));
        assert_eq!(stuck[0].task_status, Some("in_progress".to_string()));
        assert_eq!(
            stuck[0].recommended_action,
            Some(RecommendedAction::NudgeWorker)
        );
    }

    #[test]
    fn task_aware_detect_stuck_request_review() {
        let config = StuckDetectionConfig::new(5);
        let detector = TaskAwareStuckDetector::new(StuckDetector::new(config));

        let mut agent = make_agent("session-1", "agent-1");
        agent.last_event_at = Utc::now() - chrono::Duration::minutes(10);

        let task = TaskContext {
            task_id: "T001".to_string(),
            status: "in_review".to_string(),
            assignee_agent_id: Some("agent-1".to_string()),
            has_active_review: false,
            updated_at: Utc::now(),
        };

        let stuck = detector.detect_stuck_with_tasks(&[&agent], &[task]);
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0].task_status, Some("in_review".to_string()));
        assert_eq!(
            stuck[0].recommended_action,
            Some(RecommendedAction::RequestReview)
        );
    }

    #[test]
    fn task_aware_detect_stuck_check_review() {
        let config = StuckDetectionConfig::new(5);
        let detector = TaskAwareStuckDetector::new(StuckDetector::new(config));

        let mut agent = make_agent("session-1", "agent-1");
        agent.last_event_at = Utc::now() - chrono::Duration::minutes(10);

        let task = TaskContext {
            task_id: "T001".to_string(),
            status: "in_review".to_string(),
            assignee_agent_id: Some("agent-1".to_string()),
            has_active_review: true,
            updated_at: Utc::now(),
        };

        let stuck = detector.detect_stuck_with_tasks(&[&agent], &[task]);
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0].task_status, Some("in_review".to_string()));
        assert_eq!(
            stuck[0].recommended_action,
            Some(RecommendedAction::CheckReview)
        );
    }

    #[test]
    fn task_aware_mixed_case_status() {
        let config = StuckDetectionConfig::new(5);
        let detector = TaskAwareStuckDetector::new(StuckDetector::new(config));

        let mut agent = make_agent("session-1", "agent-1");
        agent.last_event_at = Utc::now() - chrono::Duration::minutes(10);

        let task = TaskContext {
            task_id: "T001".to_string(),
            status: "InReview".to_string(),
            assignee_agent_id: Some("agent-1".to_string()),
            has_active_review: false,
            updated_at: Utc::now(),
        };

        let stuck = detector.detect_stuck_with_tasks(&[&agent], &[task]);
        assert_eq!(stuck.len(), 1);
        assert_eq!(
            stuck[0].recommended_action,
            Some(RecommendedAction::RequestReview)
        );
    }

    #[test]
    fn task_aware_pattern_only_still_nudges() {
        let config = StuckDetectionConfig {
            pattern_threshold: 3,
            pattern_window_minutes: 5,
            ..Default::default()
        };
        let detector = TaskAwareStuckDetector::new(StuckDetector::new(config));

        let mut agent = make_agent("session-1", "agent-1");
        agent.record_message("error: connection failed".to_string());
        agent.record_message("error: connection failed".to_string());
        agent.record_message("error: connection failed".to_string());

        let task = TaskContext {
            task_id: "T001".to_string(),
            status: "in_progress".to_string(),
            assignee_agent_id: Some("agent-1".to_string()),
            has_active_review: false,
            updated_at: Utc::now(),
        };

        let stuck = detector.detect_stuck_with_tasks(&[&agent], &[task]);
        assert_eq!(stuck.len(), 1);
        assert_eq!(
            stuck[0].recommended_action,
            Some(RecommendedAction::NudgeWorker)
        );
    }

    #[test]
    fn task_aware_detect_stuck_escalate() {
        let config = StuckDetectionConfig::new(5);
        let detector = TaskAwareStuckDetector::new(StuckDetector::new(config));

        let mut agent = make_agent("session-1", "agent-1");
        agent.last_event_at = Utc::now() - chrono::Duration::minutes(10);

        let task = TaskContext {
            task_id: "T001".to_string(),
            status: "approved".to_string(),
            assignee_agent_id: Some("agent-1".to_string()),
            has_active_review: false,
            updated_at: Utc::now(),
        };

        let stuck = detector.detect_stuck_with_tasks(&[&agent], &[task]);
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0].task_status, Some("approved".to_string()));
        assert_eq!(
            stuck[0].recommended_action,
            Some(RecommendedAction::Escalate)
        );
    }

    #[test]
    fn reconcile_finds_orphaned_in_review() {
        let config = StuckDetectionConfig::new(5);
        let detector = TaskAwareStuckDetector::new(StuckDetector::new(config));

        let task = TaskContext {
            task_id: "T001".to_string(),
            status: "in_review".to_string(),
            assignee_agent_id: Some("dead-agent".to_string()),
            has_active_review: false,
            updated_at: Utc::now() - chrono::Duration::minutes(15),
        };

        let stuck = detector.reconcile_orphaned_tasks(&[], &[task]);
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0].task_id, Some("T001".to_string()));
        assert_eq!(
            stuck[0].recommended_action,
            Some(RecommendedAction::Reassign)
        );
    }

    #[test]
    fn reconcile_finds_orphaned_approved() {
        let config = StuckDetectionConfig::new(5);
        let detector = TaskAwareStuckDetector::new(StuckDetector::new(config));

        let task = TaskContext {
            task_id: "T001".to_string(),
            status: "approved".to_string(),
            assignee_agent_id: Some("dead-agent".to_string()),
            has_active_review: false,
            updated_at: Utc::now() - chrono::Duration::minutes(15),
        };

        let stuck = detector.reconcile_orphaned_tasks(&[], &[task]);
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0].task_id, Some("T001".to_string()));
        assert_eq!(
            stuck[0].recommended_action,
            Some(RecommendedAction::Escalate)
        );
    }

    #[test]
    fn reconcile_ignores_healthy_tasks() {
        let config = StuckDetectionConfig::new(5);
        let detector = TaskAwareStuckDetector::new(StuckDetector::new(config));

        let mut agent = make_agent("session-1", "agent-1");
        agent.is_alive = true;

        let task = TaskContext {
            task_id: "T001".to_string(),
            status: "in_progress".to_string(),
            assignee_agent_id: Some("agent-1".to_string()),
            has_active_review: false,
            updated_at: Utc::now(),
        };

        let stuck = detector.reconcile_orphaned_tasks(&[&agent], &[task]);
        assert!(stuck.is_empty(), "Healthy task should not be orphaned");
    }

    #[test]
    fn existing_tests_still_pass() {
        let config = StuckDetectionConfig::new(5);
        let detector = StuckDetector::new(config);

        let mut agent = make_agent("session-1", "agent-1");
        agent.last_event_at = Utc::now() - chrono::Duration::minutes(10);

        let stuck = detector.detect_stuck(&[&agent]);
        assert_eq!(stuck.len(), 1);

        let config = StuckDetectionConfig {
            pattern_threshold: 3,
            pattern_window_minutes: 5,
            ..Default::default()
        };
        let detector = StuckDetector::new(config);

        let mut agent = make_agent("session-1", "agent-1");
        agent.record_message("error: connection failed".to_string());
        agent.record_message("error: connection failed".to_string());
        agent.record_message("error: connection failed".to_string());

        let stuck = detector.detect_stuck(&[&agent]);
        assert_eq!(stuck.len(), 1);
        assert!(stuck[0].pattern.is_some());
    }
}

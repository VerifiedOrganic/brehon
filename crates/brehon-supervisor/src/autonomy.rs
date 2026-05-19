//! Autonomy configuration for the supervisor.
//!
//! Controls when the AI is invoked for decision-making.

use serde::{Deserialize, Serialize};

use brehon_types::{AutonomyLevel, DecisionKind, TaskStatus};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutonomyConfig {
    pub level: AutonomyLevel,
}

impl Default for AutonomyConfig {
    fn default() -> Self {
        Self {
            level: AutonomyLevel::Guided,
        }
    }
}

impl AutonomyConfig {
    pub fn new(level: AutonomyLevel) -> Self {
        Self { level }
    }

    pub fn should_invoke_ai_for_planning(&self) -> bool {
        matches!(self.level, AutonomyLevel::Full | AutonomyLevel::Guided)
    }

    pub fn should_invoke_ai_for_assignment(&self, task_status: TaskStatus) -> bool {
        match self.level {
            AutonomyLevel::Full => true,
            AutonomyLevel::Guided => task_status == TaskStatus::Pending,
            AutonomyLevel::Minimal => false,
        }
    }

    pub fn should_invoke_ai_for_stuck(&self) -> bool {
        true
    }

    pub fn should_invoke_ai_for_failure(&self) -> bool {
        true
    }

    pub fn should_invoke_ai_for_deadlock(&self) -> bool {
        true
    }

    pub fn should_invoke_ai_for_heartbeat(&self) -> bool {
        matches!(self.level, AutonomyLevel::Full | AutonomyLevel::Guided)
    }

    pub fn should_invoke_ai_for_sanity_check(&self) -> bool {
        matches!(self.level, AutonomyLevel::Full)
    }

    pub fn should_invoke_ai_for(&self, kind: DecisionKind) -> bool {
        match kind {
            DecisionKind::PlanExecution => self.should_invoke_ai_for_planning(),
            DecisionKind::AssignWorker => self.should_invoke_ai_for_assignment(TaskStatus::Pending),
            DecisionKind::StuckGuidance => self.should_invoke_ai_for_stuck(),
            DecisionKind::ReviewDeadlock => self.should_invoke_ai_for_deadlock(),
            DecisionKind::MergeConflict => self.should_invoke_ai_for_failure(),
            DecisionKind::HeartbeatCheck => self.should_invoke_ai_for_heartbeat(),
            DecisionKind::NextAction => self.should_invoke_ai_for_planning(),
            DecisionKind::ErrorHandler => self.should_invoke_ai_for_failure(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autonomy_default_is_guided() {
        let config = AutonomyConfig::default();
        assert_eq!(config.level, AutonomyLevel::Guided);
    }

    #[test]
    fn full_level_allows_all() {
        let config = AutonomyConfig::new(AutonomyLevel::Full);

        assert!(config.should_invoke_ai_for_planning());
        assert!(config.should_invoke_ai_for_assignment(TaskStatus::Pending));
        assert!(config.should_invoke_ai_for_stuck());
        assert!(config.should_invoke_ai_for_failure());
        assert!(config.should_invoke_ai_for_deadlock());
        assert!(config.should_invoke_ai_for_heartbeat());
        assert!(config.should_invoke_ai_for_sanity_check());
    }

    #[test]
    fn guided_level_allows_subset() {
        let config = AutonomyConfig::new(AutonomyLevel::Guided);

        assert!(config.should_invoke_ai_for_planning());
        assert!(config.should_invoke_ai_for_assignment(TaskStatus::Pending));
        assert!(config.should_invoke_ai_for_stuck());
        assert!(config.should_invoke_ai_for_failure());
        assert!(config.should_invoke_ai_for_deadlock());
        assert!(config.should_invoke_ai_for_heartbeat());
        assert!(!config.should_invoke_ai_for_sanity_check());
    }

    #[test]
    fn minimal_level_allows_minimal() {
        let config = AutonomyConfig::new(AutonomyLevel::Minimal);

        assert!(!config.should_invoke_ai_for_planning());
        assert!(!config.should_invoke_ai_for_assignment(TaskStatus::Pending));
        assert!(config.should_invoke_ai_for_stuck());
        assert!(config.should_invoke_ai_for_failure());
        assert!(config.should_invoke_ai_for_deadlock());
        assert!(!config.should_invoke_ai_for_heartbeat());
        assert!(!config.should_invoke_ai_for_sanity_check());
    }

    #[test]
    fn decision_kind_mapping() {
        let config = AutonomyConfig::new(AutonomyLevel::Full);

        assert!(config.should_invoke_ai_for(DecisionKind::PlanExecution));
        assert!(config.should_invoke_ai_for(DecisionKind::AssignWorker));
        assert!(config.should_invoke_ai_for(DecisionKind::StuckGuidance));
        assert!(config.should_invoke_ai_for(DecisionKind::HeartbeatCheck));

        let config = AutonomyConfig::new(AutonomyLevel::Minimal);

        assert!(!config.should_invoke_ai_for(DecisionKind::PlanExecution));
        assert!(!config.should_invoke_ai_for(DecisionKind::AssignWorker));
        assert!(config.should_invoke_ai_for(DecisionKind::StuckGuidance));
        assert!(!config.should_invoke_ai_for(DecisionKind::HeartbeatCheck));
    }
}

//! Human escalation for the supervisor.
//!
//! Handles escalation to human operators when AI retries fail.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use tracing::{debug, warn};

use brehon_ports::{ModalAction, Notification};
use brehon_ports::{NotificationSink, PortError};

#[derive(Debug, Clone)]
pub struct EscalationConfig {
    pub max_retries: u32,
    pub timeout_minutes: u32,
    pub notify_method: NotifyMethod,
}

impl Default for EscalationConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            timeout_minutes: 30,
            notify_method: NotifyMethod::Terminal,
        }
    }
}

impl EscalationConfig {
    pub fn new(max_retries: u32) -> Self {
        Self {
            max_retries,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyMethod {
    Terminal,
    Webhook,
    None,
}

#[derive(Debug, Clone)]
pub struct EscalationRecord {
    pub reason: String,
    pub context: String,
    pub retry_count: u32,
    pub escalated_at: DateTime<Utc>,
    pub resolved: bool,
}

pub struct EscalationManager {
    config: EscalationConfig,
    notifications: Option<Arc<dyn NotificationSink>>,
    escalations: RwLock<Vec<EscalationRecord>>,
    retry_counts: RwLock<std::collections::HashMap<String, u32>>,
}

impl EscalationManager {
    pub fn new(config: EscalationConfig) -> Self {
        Self {
            config,
            notifications: None,
            escalations: RwLock::new(Vec::new()),
            retry_counts: RwLock::new(std::collections::HashMap::new()),
        }
    }

    pub fn with_notifications(mut self, notifications: Arc<dyn NotificationSink>) -> Self {
        self.notifications = Some(notifications);
        self
    }

    pub fn should_escalate(&self, decision_id: &str) -> bool {
        let counts = self.retry_counts.read();
        let count = counts.get(decision_id).copied().unwrap_or(0);
        count >= self.config.max_retries
    }

    pub fn record_retry(&self, decision_id: &str) -> u32 {
        let mut counts = self.retry_counts.write();
        let count = counts.entry(decision_id.to_string()).or_insert(0);
        *count += 1;
        *count
    }

    pub fn reset_retry(&self, decision_id: &str) {
        self.retry_counts.write().remove(decision_id);
    }

    pub fn escalate(&self, reason: &str, context: &str) -> Result<(), PortError> {
        warn!(reason = reason, "Escalating to human");

        let record = EscalationRecord {
            reason: reason.to_string(),
            context: context.to_string(),
            retry_count: self.retry_counts.read().values().sum(),
            escalated_at: Utc::now(),
            resolved: false,
        };

        self.escalations.write().push(record);

        if let Some(ref notifications) = self.notifications {
            let message = format!(
                "ESCALATION REQUIRED\n\nReason: {}\n\nContext:\n{}",
                reason, context
            );

            let actions = vec![
                ModalAction::new("Acknowledge", "ack"),
                ModalAction::new("View Details", "details"),
                ModalAction::new("Take Over", "takeover"),
            ];

            notifications.modal(&message, actions)?;
            notifications.toast(Notification::error(reason))?;
        }

        debug!("Escalation recorded");
        Ok(())
    }

    pub fn resolve(&self, reason: &str) {
        let mut escalations = self.escalations.write();
        for record in escalations.iter_mut().rev() {
            if !record.resolved && record.reason == reason {
                record.resolved = true;
                break;
            }
        }
    }

    pub fn pending_escalations(&self) -> Vec<EscalationRecord> {
        self.escalations
            .read()
            .iter()
            .filter(|e| !e.resolved)
            .cloned()
            .collect()
    }

    pub fn all_escalations(&self) -> Vec<EscalationRecord> {
        self.escalations.read().clone()
    }

    pub fn clear_escalations(&self) {
        self.escalations.write().clear();
        self.retry_counts.write().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_test_harness::RecordingNotificationSink;

    #[test]
    fn should_escalate_after_max_retries() {
        let config = EscalationConfig::new(3);
        let manager = EscalationManager::new(config);

        assert!(!manager.should_escalate("decision-1"));

        manager.record_retry("decision-1");
        assert!(!manager.should_escalate("decision-1"));

        manager.record_retry("decision-1");
        manager.record_retry("decision-1");
        assert!(manager.should_escalate("decision-1"));
    }

    #[test]
    fn reset_retry() {
        let manager = EscalationManager::new(EscalationConfig::new(3));

        manager.record_retry("decision-1");
        manager.record_retry("decision-1");
        assert_eq!(manager.record_retry("decision-1"), 3);

        manager.reset_retry("decision-1");
        assert!(!manager.should_escalate("decision-1"));
    }

    #[test]
    fn escalate_creates_record() {
        let manager = EscalationManager::new(EscalationConfig::default());

        manager.escalate("Test reason", "Test context").unwrap();

        let pending = manager.pending_escalations();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].reason, "Test reason");
        assert!(!pending[0].resolved);
    }

    #[test]
    fn resolve_escalation() {
        let manager = EscalationManager::new(EscalationConfig::default());

        manager.escalate("Test reason", "Test context").unwrap();
        assert_eq!(manager.pending_escalations().len(), 1);

        manager.resolve("Test reason");
        assert_eq!(manager.pending_escalations().len(), 0);
    }

    #[test]
    fn escalate_with_notifications() {
        let notifications = Arc::new(RecordingNotificationSink::new());
        let manager = EscalationManager::new(EscalationConfig::default())
            .with_notifications(notifications.clone());

        manager.escalate("Test escalation", "Test context").unwrap();

        let modals = notifications.modals();
        assert_eq!(modals.len(), 1);
        assert!(modals[0].1.len() >= 2);
    }
}

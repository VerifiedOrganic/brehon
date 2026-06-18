//! External operator notification types and configuration.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Top-level external notification configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExternalNotificationsConfig {
    /// Master switch. When false, no external notifications are delivered.
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    /// Provider-specific configuration.
    #[serde(
        default,
        skip_serializing_if = "NotificationProvidersConfig::is_default"
    )]
    pub providers: NotificationProvidersConfig,
    /// Event subscriptions. Empty means no events are delivered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscriptions: Vec<NotificationSubscriptionConfig>,
}

impl ExternalNotificationsConfig {
    /// Returns true when this config is exactly the disabled default.
    pub fn is_default(&self) -> bool {
        !self.enabled && self.providers.is_default() && self.subscriptions.is_empty()
    }

    /// Returns true when at least one subscription wants this event.
    pub fn has_subscriber_for(&self, event: NotificationEventKind) -> bool {
        self.enabled
            && self
                .subscriptions
                .iter()
                .any(|subscription| subscription.matches_event(event))
    }
}

/// External notification provider configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotificationProvidersConfig {
    /// Telegram Bot API delivery.
    #[serde(
        default,
        skip_serializing_if = "TelegramNotificationConfig::is_default"
    )]
    pub telegram: TelegramNotificationConfig,
}

impl NotificationProvidersConfig {
    /// Returns true when no provider is enabled or customized.
    pub fn is_default(&self) -> bool {
        self.telegram.is_default()
    }
}

/// Telegram Bot API notification configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TelegramNotificationConfig {
    /// Enable Telegram delivery.
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    /// Environment variable that contains the bot token.
    #[serde(default = "default_telegram_bot_token_env")]
    pub bot_token_env: String,
    /// Environment variable that contains the target chat id.
    #[serde(default = "default_telegram_chat_id_env")]
    pub chat_id_env: String,
    /// Per-message send timeout in seconds.
    #[serde(default = "default_notification_send_timeout_secs")]
    pub send_timeout_secs: u64,
}

impl Default for TelegramNotificationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token_env: default_telegram_bot_token_env(),
            chat_id_env: default_telegram_chat_id_env(),
            send_timeout_secs: default_notification_send_timeout_secs(),
        }
    }
}

impl TelegramNotificationConfig {
    /// Returns true when Telegram is disabled and all defaults are intact.
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// One provider subscription to one or more event kinds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotificationSubscriptionConfig {
    /// Provider that should receive matching events.
    pub provider: NotificationProviderKind,
    /// Event kinds delivered to this provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<NotificationEventKind>,
}

impl NotificationSubscriptionConfig {
    /// Returns true when this subscription includes `event`.
    pub fn matches_event(&self, event: NotificationEventKind) -> bool {
        self.events.contains(&event)
    }
}

/// External notification provider.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum NotificationProviderKind {
    /// Telegram Bot API.
    Telegram,
}

/// Operator notification event kind.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum NotificationEventKind {
    /// A Brehon run started.
    #[serde(rename = "run.started")]
    RunStarted,
    /// A Brehon run shut down gracefully.
    #[serde(rename = "run.shutdown")]
    RunShutdown,
    /// Brehon detected a runtime crash or unclean recovery signal.
    #[serde(rename = "run.crash_detected")]
    RunCrashDetected,
    /// A worker completed task handoff to review.
    #[serde(rename = "task.completed")]
    TaskCompleted,
    /// A task reached a terminal merged/closed state.
    #[serde(rename = "task.closed")]
    TaskClosed,
    /// A task became blocked.
    #[serde(rename = "task.blocked")]
    TaskBlocked,
    /// An epic reached terminal completion.
    #[serde(rename = "epic.completed")]
    EpicCompleted,
    /// A post-review integration started.
    #[serde(rename = "integration.started")]
    IntegrationStarted,
    /// A post-review integration completed.
    #[serde(rename = "integration.completed")]
    IntegrationCompleted,
    /// A post-review integration hit an actionable failure.
    #[serde(rename = "integration.failed")]
    IntegrationFailed,
    /// A review approved a task.
    #[serde(rename = "review.approved")]
    ReviewApproved,
    /// A review requested changes or rejected a task.
    #[serde(rename = "review.rejected")]
    ReviewRejected,
    /// A soft budget warning fired.
    #[serde(rename = "budget.warning")]
    BudgetWarning,
    /// The hard budget kill-switch fired.
    #[serde(rename = "budget.kill_switch")]
    BudgetKillSwitch,
    /// Startup/runtime recovery performed an operator-visible action.
    #[serde(rename = "recovery.performed")]
    RecoveryPerformed,
    /// An agent was detected as stalled.
    #[serde(rename = "agent.stalled")]
    AgentStalled,
}

impl NotificationEventKind {
    /// Stable config/string representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RunStarted => "run.started",
            Self::RunShutdown => "run.shutdown",
            Self::RunCrashDetected => "run.crash_detected",
            Self::TaskCompleted => "task.completed",
            Self::TaskClosed => "task.closed",
            Self::TaskBlocked => "task.blocked",
            Self::EpicCompleted => "epic.completed",
            Self::IntegrationStarted => "integration.started",
            Self::IntegrationCompleted => "integration.completed",
            Self::IntegrationFailed => "integration.failed",
            Self::ReviewApproved => "review.approved",
            Self::ReviewRejected => "review.rejected",
            Self::BudgetWarning => "budget.warning",
            Self::BudgetKillSwitch => "budget.kill_switch",
            Self::RecoveryPerformed => "recovery.performed",
            Self::AgentStalled => "agent.stalled",
        }
    }
}

impl std::fmt::Display for NotificationEventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Operator notification severity.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum NotificationSeverity {
    /// Informational state change.
    Info,
    /// State change that may need operator attention.
    Warning,
    /// State change that stopped work or needs immediate attention.
    Critical,
}

impl NotificationSeverity {
    /// Stable lowercase representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// A provider-neutral operator notification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotificationEvent {
    /// Event kind used for subscription matching.
    pub kind: NotificationEventKind,
    /// Severity used in rendered messages.
    pub severity: NotificationSeverity,
    /// Short title.
    pub title: String,
    /// Human-readable summary.
    pub message: String,
    /// Structured detail fields for provider renderers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, String>,
}

impl NotificationEvent {
    /// Create an event with no detail fields.
    pub fn new(
        kind: NotificationEventKind,
        severity: NotificationSeverity,
        title: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            severity,
            title: title.into(),
            message: message.into(),
            fields: BTreeMap::new(),
        }
    }

    /// Attach a detail field when the value is not empty.
    #[must_use]
    pub fn field(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let key = key.into();
        let value = value.into();
        if !key.trim().is_empty() && !value.trim().is_empty() {
            self.fields.insert(key, value);
        }
        self
    }
}

fn default_telegram_bot_token_env() -> String {
    "BREHON_TELEGRAM_BOT_TOKEN".to_string()
}

fn default_telegram_chat_id_env() -> String {
    "BREHON_TELEGRAM_CHAT_ID".to_string()
}

const fn default_notification_send_timeout_secs() -> u64 {
    5
}

const fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_kind_serde_uses_dotted_names() {
        let value = serde_json::to_string(&NotificationEventKind::TaskCompleted)
            .expect("serialize event kind");
        assert_eq!(value, "\"task.completed\"");
        let parsed: NotificationEventKind =
            serde_json::from_str("\"budget.kill_switch\"").expect("parse event kind");
        assert_eq!(parsed, NotificationEventKind::BudgetKillSwitch);
    }

    #[test]
    fn subscription_matches_only_listed_events() {
        let subscription = NotificationSubscriptionConfig {
            provider: NotificationProviderKind::Telegram,
            events: vec![NotificationEventKind::TaskCompleted],
        };
        assert!(subscription.matches_event(NotificationEventKind::TaskCompleted));
        assert!(!subscription.matches_event(NotificationEventKind::TaskClosed));
    }
}

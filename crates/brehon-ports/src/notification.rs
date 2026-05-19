//! NotificationSink trait for TUI notifications.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::PortError;

/// Notification level.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum NotificationLevel {
    /// Informational message.
    Info,
    /// Warning message.
    Warning,
    /// Error message.
    Error,
}

/// A notification to display.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Notification {
    /// Notification message.
    pub message: String,
    /// Notification level.
    pub level: NotificationLevel,
    /// When the notification was created.
    pub timestamp: DateTime<Utc>,
}

impl Notification {
    /// Create a new notification.
    pub fn new(message: impl Into<String>, level: NotificationLevel) -> Self {
        Self {
            message: message.into(),
            level,
            timestamp: Utc::now(),
        }
    }

    /// Create an info notification.
    pub fn info(message: impl Into<String>) -> Self {
        Self::new(message, NotificationLevel::Info)
    }

    /// Create a warning notification.
    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(message, NotificationLevel::Warning)
    }

    /// Create an error notification.
    pub fn error(message: impl Into<String>) -> Self {
        Self::new(message, NotificationLevel::Error)
    }
}

/// Identifier for a tab in the TUI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct TabId(pub String);

impl TabId {
    /// Create a new tab identifier.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the tab identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Action for a modal dialog.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModalAction {
    /// Action label for the button.
    pub label: String,
    /// Action identifier.
    pub action: String,
}

impl ModalAction {
    /// Create a new modal action.
    pub fn new(label: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            action: action.into(),
        }
    }
}

/// Trait for sending notifications to the TUI.
///
/// This trait abstracts notification delivery so that different
/// notification backends can be used (TUI, log file, external service, etc.).
///
/// Implementations should:
/// - Be non-blocking (notification delivery shouldn't block)
/// - Handle delivery failures gracefully
/// - Support concurrent access
pub trait NotificationSink: Send + Sync {
    /// Show a toast notification.
    ///
    /// Displays a temporary notification that dismisses automatically
    /// after a configured duration.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Notification` if the notification cannot be displayed.
    fn toast(&self, notification: Notification) -> Result<(), PortError>;

    /// Flash a tab indicator.
    ///
    /// Causes a tab to flash/highlight to draw attention.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Notification` if the flash operation fails.
    fn flash_tab(&self, tab: TabId) -> Result<(), PortError>;

    /// Show a modal dialog.
    ///
    /// Displays a modal dialog with the given message and action buttons.
    /// The dialog blocks until the user dismisses it.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Notification` if the modal cannot be displayed.
    fn modal(&self, message: &str, actions: Vec<ModalAction>) -> Result<(), PortError>;

    /// Log a notification to history.
    ///
    /// Adds the notification to the persistent notification history
    /// without displaying it.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Notification` if logging fails.
    fn log(&self, notification: Notification) -> Result<(), PortError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_creation() {
        let n1 = Notification::info("Task completed");
        assert_eq!(n1.level, NotificationLevel::Info);
        assert_eq!(n1.message, "Task completed");

        let n2 = Notification::warning("Budget at 80%");
        assert_eq!(n2.level, NotificationLevel::Warning);

        let n3 = Notification::error("Agent crashed");
        assert_eq!(n3.level, NotificationLevel::Error);
    }

    #[test]
    fn tab_id() {
        let tab = TabId::new("workers");
        assert_eq!(tab.as_str(), "workers");
    }

    #[test]
    fn modal_action() {
        let action = ModalAction::new("Confirm", "confirm");
        assert_eq!(action.label, "Confirm");
        assert_eq!(action.action, "confirm");
    }
}

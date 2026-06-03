//! Recording NotificationSink implementation for testing.
//!
//! Records all notifications for assertions in tests.

use std::sync::Arc;

use brehon_ports::{ModalAction, Notification, NotificationSink, PortError, TabId};
use parking_lot::RwLock;

/// Recorded notification for assertions.
#[derive(Debug, Clone)]
pub struct RecordedNotification {
    pub notification: Notification,
    pub kind: RecordedKind,
}

#[derive(Debug, Clone)]
pub enum RecordedKind {
    Toast,
    Flash { tab_id: String },
    Modal { actions: Vec<ModalAction> },
    Log,
}

/// Recording notification sink for testing.
///
/// Records all notifications so tests can query them.
#[derive(Debug, Clone)]
pub struct RecordingNotificationSink {
    inner: Arc<RwLock<Vec<RecordedNotification>>>,
}

impl RecordingNotificationSink {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn notifications(&self) -> Vec<RecordedNotification> {
        self.inner.read().clone()
    }

    pub fn toasts(&self) -> Vec<Notification> {
        self.inner
            .read()
            .iter()
            .filter(|n| matches!(n.kind, RecordedKind::Toast))
            .map(|n| n.notification.clone())
            .collect()
    }

    pub fn modals(&self) -> Vec<(Notification, Vec<ModalAction>)> {
        self.inner
            .read()
            .iter()
            .filter_map(|n| match &n.kind {
                RecordedKind::Modal { actions } => Some((n.notification.clone(), actions.clone())),
                _ => None,
            })
            .collect()
    }

    pub fn was_toast_called(&self) -> bool {
        self.inner
            .read()
            .iter()
            .any(|n| matches!(n.kind, RecordedKind::Toast))
    }

    pub fn last_modal(&self) -> Option<(String, Vec<ModalAction>)> {
        self.inner.read().iter().rev().find_map(|n| match &n.kind {
            RecordedKind::Modal { actions } => {
                Some((n.notification.message.clone(), actions.clone()))
            }
            _ => None,
        })
    }

    pub fn flash_count(&self) -> usize {
        self.inner
            .read()
            .iter()
            .filter(|n| matches!(n.kind, RecordedKind::Flash { .. }))
            .count()
    }

    pub fn clear(&self) {
        self.inner.write().clear();
    }

    pub fn count(&self) -> usize {
        self.inner.read().len()
    }
}

impl Default for RecordingNotificationSink {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationSink for RecordingNotificationSink {
    fn toast(&self, notification: Notification) -> Result<(), PortError> {
        self.inner.write().push(RecordedNotification {
            notification,
            kind: RecordedKind::Toast,
        });
        Ok(())
    }

    fn flash_tab(&self, tab: TabId) -> Result<(), PortError> {
        self.inner.write().push(RecordedNotification {
            notification: Notification::info("flash"),
            kind: RecordedKind::Flash {
                tab_id: tab.as_str().to_string(),
            },
        });
        Ok(())
    }

    fn modal(&self, message: &str, actions: Vec<ModalAction>) -> Result<(), PortError> {
        self.inner.write().push(RecordedNotification {
            notification: Notification::warning(message),
            kind: RecordedKind::Modal { actions },
        });
        Ok(())
    }

    fn log(&self, notification: Notification) -> Result<(), PortError> {
        self.inner.write().push(RecordedNotification {
            notification,
            kind: RecordedKind::Log,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_notification() {
        let sink = RecordingNotificationSink::new();

        sink.toast(Notification::info("test")).unwrap();

        assert!(sink.was_toast_called());
        assert_eq!(sink.toasts().len(), 1);
    }

    #[test]
    fn modal_notification() {
        let sink = RecordingNotificationSink::new();

        sink.modal(
            "Are you sure?",
            vec![
                ModalAction::new("Yes", "confirm"),
                ModalAction::new("No", "cancel"),
            ],
        )
        .unwrap();

        let modals = sink.modals();
        assert_eq!(modals.len(), 1);
        assert_eq!(modals[0].1.len(), 2);
    }

    #[test]
    fn flash_tab() {
        let sink = RecordingNotificationSink::new();

        sink.flash_tab(TabId::new("workers")).unwrap();
        sink.flash_tab(TabId::new("reviewers")).unwrap();

        assert_eq!(sink.flash_count(), 2);
    }

    #[test]
    fn last_modal() {
        let sink = RecordingNotificationSink::new();

        sink.modal("First", vec![ModalAction::new("OK", "ok")])
            .unwrap();
        sink.modal("Second", vec![ModalAction::new("Cancel", "cancel")])
            .unwrap();

        let (message, _) = sink.last_modal().unwrap();
        assert_eq!(message, "Second");
    }

    #[test]
    fn clear() {
        let sink = RecordingNotificationSink::new();

        sink.toast(Notification::info("test")).unwrap();
        assert_eq!(sink.count(), 1);

        sink.clear();
        assert_eq!(sink.count(), 0);
    }
}

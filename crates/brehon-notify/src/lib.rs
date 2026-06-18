//! External operator notification delivery.
//!
//! The orchestration crates call this crate at ownership boundaries and never
//! wait on live delivery. Provider failures are logged and do not block work.

use std::time::Duration;

use brehon_types::{
    ExternalNotificationsConfig, NotificationEvent, NotificationEventKind,
    NotificationProviderKind, TelegramNotificationConfig,
};
use reqwest::StatusCode;
use serde::Serialize;
use thiserror::Error;

/// Notification delivery error.
#[derive(Debug, Error)]
pub enum NotificationError {
    /// No provider is enabled for the subscribed event.
    #[error("no enabled provider for notification event {0}")]
    NoEnabledProvider(NotificationEventKind),
    /// A required secret environment variable is unset or empty.
    #[error("required notification environment variable {0} is unset or empty")]
    MissingSecret(String),
    /// A configured secret environment variable is not unicode.
    #[error("required notification environment variable {0} is not valid unicode")]
    InvalidSecret(String),
    /// HTTP client creation failed.
    #[error("failed to create notification HTTP client: {0}")]
    Client(String),
    /// Provider request failed.
    #[error("notification provider request failed: {0}")]
    Request(String),
    /// Provider returned a non-success status.
    #[error("notification provider returned HTTP {status}: {body}")]
    ProviderStatus {
        /// HTTP status.
        status: StatusCode,
        /// Bounded response body.
        body: String,
    },
}

/// Summary of a best-effort notification dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryReport {
    /// Number of provider sends attempted.
    pub attempted: usize,
    /// Number of provider sends that succeeded.
    pub delivered: usize,
}

impl DeliveryReport {
    const fn empty() -> Self {
        Self {
            attempted: 0,
            delivered: 0,
        }
    }
}

/// Spawn best-effort delivery on the provided runtime.
///
/// Returns `false` when config/subscriptions mean the event is ignored. Delivery
/// failures are logged by the background task and never surface to callers.
#[must_use]
pub fn spawn_notification(
    runtime: &tokio::runtime::Handle,
    config: ExternalNotificationsConfig,
    event: NotificationEvent,
) -> bool {
    if !config.has_subscriber_for(event.kind) {
        return false;
    }

    runtime.spawn(async move {
        if let Err(err) = deliver_notification(config, event).await {
            tracing::warn!(error = %err, "external notification delivery failed");
        }
    });
    true
}

/// Deliver a notification immediately with provider-specific bounded timeouts.
///
/// This is intended for shutdown paths where the process may exit soon after
/// dispatching the event. Live orchestration paths should call
/// [`spawn_notification`] instead.
pub async fn deliver_notification(
    config: ExternalNotificationsConfig,
    event: NotificationEvent,
) -> Result<DeliveryReport, NotificationError> {
    if !config.enabled || !config.has_subscriber_for(event.kind) {
        return Ok(DeliveryReport::empty());
    }

    let mut report = DeliveryReport::empty();
    let mut saw_enabled_provider = false;
    for subscription in &config.subscriptions {
        if !subscription.matches_event(event.kind) {
            continue;
        }
        match subscription.provider {
            NotificationProviderKind::Telegram => {
                if !config.providers.telegram.enabled {
                    continue;
                }
                saw_enabled_provider = true;
                report.attempted += 1;
                send_telegram(&config.providers.telegram, &event).await?;
                report.delivered += 1;
            }
        }
    }

    if report.attempted == 0 && !saw_enabled_provider {
        return Err(NotificationError::NoEnabledProvider(event.kind));
    }

    Ok(report)
}

#[derive(Debug, Serialize)]
struct TelegramSendMessage<'a> {
    chat_id: &'a str,
    text: &'a str,
    disable_web_page_preview: bool,
}

async fn send_telegram(
    config: &TelegramNotificationConfig,
    event: &NotificationEvent,
) -> Result<(), NotificationError> {
    let token = secret_env(&config.bot_token_env)?;
    let chat_id = secret_env(&config.chat_id_env)?;
    let timeout = Duration::from_secs(config.send_timeout_secs.max(1));
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|err| NotificationError::Client(err.to_string()))?;
    let text = render_text(event);
    let url = format!(
        "https://api.telegram.org/bot{}/sendMessage",
        token.trim_matches('/')
    );
    let response = client
        .post(url)
        .json(&TelegramSendMessage {
            chat_id: &chat_id,
            text: &text,
            disable_web_page_preview: true,
        })
        .send()
        .await
        .map_err(|err| NotificationError::Request(err.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|err| format!("failed to read provider response body: {err}"));
        return Err(NotificationError::ProviderStatus {
            status,
            body: truncate_body(&body),
        });
    }
    Ok(())
}

fn secret_env(name: &str) -> Result<String, NotificationError> {
    let trimmed = name.trim();
    match std::env::var(trimmed) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        Ok(_) | Err(std::env::VarError::NotPresent) => {
            Err(NotificationError::MissingSecret(trimmed.to_string()))
        }
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(NotificationError::InvalidSecret(trimmed.to_string()))
        }
    }
}

/// Render a provider-neutral plain-text notification.
#[must_use]
pub fn render_text(event: &NotificationEvent) -> String {
    let mut lines = vec![
        format!(
            "[Brehon] {} ({})",
            event.title.trim(),
            event.severity.as_str()
        ),
        event.message.trim().to_string(),
        format!("event: {}", event.kind),
    ];
    if !event.fields.is_empty() {
        lines.push("details:".to_string());
        for (key, value) in &event.fields {
            lines.push(format!("- {}: {}", key.trim(), value.trim()));
        }
    }
    lines.join("\n")
}

fn truncate_body(body: &str) -> String {
    const MAX_BODY_CHARS: usize = 512;
    body.chars().take(MAX_BODY_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_types::{
        NotificationEvent, NotificationEventKind, NotificationSeverity,
        NotificationSubscriptionConfig,
    };

    fn task_event() -> NotificationEvent {
        NotificationEvent::new(
            NotificationEventKind::TaskCompleted,
            NotificationSeverity::Info,
            "Task completed",
            "Task T-1 is ready for review.",
        )
        .field("task_id", "T-1")
        .field("assignee", "worker-1")
    }

    #[test]
    fn render_text_includes_relevant_fields() {
        let rendered = render_text(&task_event());
        assert!(rendered.contains("[Brehon] Task completed (info)"));
        assert!(rendered.contains("Task T-1 is ready for review."));
        assert!(rendered.contains("event: task.completed"));
        assert!(rendered.contains("- task_id: T-1"));
        assert!(rendered.contains("- assignee: worker-1"));
    }

    #[tokio::test]
    async fn disabled_config_does_not_attempt_delivery() {
        let report = deliver_notification(ExternalNotificationsConfig::default(), task_event())
            .await
            .expect("disabled config should be a no-op");
        assert_eq!(report.attempted, 0);
        assert_eq!(report.delivered, 0);
    }

    #[tokio::test]
    async fn subscribed_but_disabled_provider_reports_no_enabled_provider() {
        let config = ExternalNotificationsConfig {
            enabled: true,
            subscriptions: vec![NotificationSubscriptionConfig {
                provider: NotificationProviderKind::Telegram,
                events: vec![NotificationEventKind::TaskCompleted],
            }],
            ..ExternalNotificationsConfig::default()
        };
        let err = deliver_notification(config, task_event())
            .await
            .expect_err("disabled provider should be explicit");
        assert!(matches!(err, NotificationError::NoEnabledProvider(_)));
    }
}

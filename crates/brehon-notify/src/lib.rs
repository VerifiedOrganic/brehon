//! External operator notification delivery.
//!
//! The orchestration crates call this crate at ownership boundaries and never
//! wait on live delivery. Provider failures are logged and do not block work.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brehon_types::{
    write_json_atomic, ExternalNotificationsConfig, NotificationEvent, NotificationEventKind,
    NotificationProviderKind, TelegramNotificationConfig,
};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
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

/// Notification outbox persistence error.
#[derive(Debug, Error)]
pub enum NotificationOutboxError {
    /// Filesystem operation failed.
    #[error("notification outbox filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    /// Stored notification JSON was malformed.
    #[error("notification outbox JSON is malformed: {0}")]
    Json(#[from] serde_json::Error),
}

/// Summary of a best-effort notification dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryReport {
    /// Number of provider sends attempted.
    pub attempted: usize,
    /// Number of provider sends that succeeded.
    pub delivered: usize,
}

/// One durable notification queued for provider delivery.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotificationOutboxItem {
    /// Stable outbox item id.
    pub id: String,
    /// Unix timestamp in milliseconds when this item was queued.
    pub queued_at_ms: u64,
    /// Number of failed delivery attempts.
    #[serde(default)]
    pub attempts: u32,
    /// Earliest Unix timestamp in milliseconds when delivery may be retried.
    #[serde(default)]
    pub next_attempt_at_ms: u64,
    /// Last bounded delivery error, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Provider-neutral event payload. Secrets are never stored here.
    pub event: NotificationEvent,
}

/// Tunables for one outbox drain pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotificationOutboxDrainOptions {
    /// Maximum files listed per drain pass.
    pub max_scan: usize,
    /// Maximum due events delivered per drain pass.
    pub max_deliveries: usize,
    /// Failed attempts before an item is moved to `failed`.
    pub max_attempts: u32,
    /// Maximum retry delay in seconds.
    pub max_backoff_secs: u64,
}

impl Default for NotificationOutboxDrainOptions {
    fn default() -> Self {
        Self {
            max_scan: 256,
            max_deliveries: 16,
            max_attempts: 12,
            max_backoff_secs: 5 * 60,
        }
    }
}

/// Summary of one durable outbox drain pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NotificationOutboxDrainReport {
    /// JSON files examined.
    pub scanned: usize,
    /// Provider sends attempted.
    pub attempted: usize,
    /// Provider sends delivered successfully.
    pub delivered: usize,
    /// Items discarded because current config does not subscribe to them.
    pub discarded: usize,
    /// Items left in the outbox because their retry backoff has not elapsed.
    pub deferred: usize,
    /// Items updated for a future retry.
    pub retry_scheduled: usize,
    /// Items moved to the failed directory.
    pub failed: usize,
    /// Malformed files moved to the failed directory.
    pub invalid: usize,
    /// Filesystem or task-join errors that prevented full processing.
    pub errors: usize,
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

/// Queue a provider-neutral notification event in `.brehon/runtime/notifications/outbox`.
///
/// This writes only the event payload and retry metadata. Provider secrets remain
/// process-local environment variables read during delivery.
pub fn enqueue_notification(
    brehon_root: &Path,
    event: NotificationEvent,
) -> Result<PathBuf, NotificationOutboxError> {
    let now_ms = unix_timestamp_ms();
    let id = uuid::Uuid::new_v4().to_string();
    let item = NotificationOutboxItem {
        id: id.clone(),
        queued_at_ms: now_ms,
        attempts: 0,
        next_attempt_at_ms: now_ms,
        last_error: None,
        event,
    };
    let path = outbox_dir(brehon_root).join(format!("{now_ms:020}-{id}.json"));
    write_json_atomic(&path, &item)?;
    Ok(path)
}

/// Drain due notification outbox items with bounded IO and provider work.
pub async fn drain_notification_outbox(
    config: ExternalNotificationsConfig,
    brehon_root: PathBuf,
    options: NotificationOutboxDrainOptions,
) -> NotificationOutboxDrainReport {
    let mut report = NotificationOutboxDrainReport::default();
    let files = match pending_outbox_files(&brehon_root, options.max_scan).await {
        Ok(files) => files,
        Err(err) => {
            report.errors += 1;
            tracing::warn!(error = %err, "failed to list notification outbox");
            return report;
        }
    };

    let mut delivered_due = 0usize;
    for path in files {
        if delivered_due >= options.max_deliveries {
            break;
        }
        report.scanned += 1;
        let item = match read_outbox_item(path.clone()).await {
            Ok(item) => item,
            Err(err) => {
                report.errors += 1;
                if move_invalid_outbox_file(&brehon_root, path.clone()).await {
                    report.invalid += 1;
                }
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "failed to read notification outbox item"
                );
                continue;
            }
        };

        let now_ms = unix_timestamp_ms();
        if item.next_attempt_at_ms > now_ms {
            report.deferred += 1;
            continue;
        }

        if !config.enabled || !config.has_subscriber_for(item.event.kind) {
            if remove_outbox_file(path.clone()).await {
                report.discarded += 1;
            } else {
                report.errors += 1;
            }
            continue;
        }

        delivered_due += 1;
        match deliver_notification(config.clone(), item.event.clone()).await {
            Ok(delivery) => {
                report.attempted += delivery.attempted;
                report.delivered += delivery.delivered;
                if !remove_outbox_file(path.clone()).await {
                    report.errors += 1;
                }
            }
            Err(err) => {
                report.attempted += 1;
                let mut failed_item = item;
                failed_item.attempts = failed_item.attempts.saturating_add(1);
                failed_item.last_error = Some(truncate_body(&err.to_string()));
                if failed_item.attempts >= options.max_attempts {
                    if move_failed_outbox_item(&brehon_root, path.clone(), failed_item).await {
                        report.failed += 1;
                    } else {
                        report.errors += 1;
                    }
                } else {
                    failed_item.next_attempt_at_ms =
                        now_ms.saturating_add(backoff_ms(failed_item.attempts, options));
                    if rewrite_outbox_item(path.clone(), failed_item).await {
                        report.retry_scheduled += 1;
                    } else {
                        report.errors += 1;
                    }
                }
            }
        }
    }

    report
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

fn outbox_dir(root: &Path) -> PathBuf {
    root.join("runtime").join("notifications").join("outbox")
}

fn failed_dir(root: &Path) -> PathBuf {
    root.join("runtime").join("notifications").join("failed")
}

async fn pending_outbox_files(
    root: &Path,
    max_scan: usize,
) -> Result<Vec<PathBuf>, NotificationOutboxError> {
    let dir = outbox_dir(root);
    let files = tokio::task::spawn_blocking(move || {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                files.push(path);
            }
        }
        files.sort();
        files.truncate(max_scan);
        Ok(files)
    })
    .await
    .map_err(join_error)?;
    files.map_err(NotificationOutboxError::Io)
}

async fn read_outbox_item(
    path: PathBuf,
) -> Result<NotificationOutboxItem, NotificationOutboxError> {
    tokio::task::spawn_blocking(move || {
        let content = std::fs::read_to_string(&path)?;
        let item = serde_json::from_str::<NotificationOutboxItem>(&content)?;
        Ok(item)
    })
    .await
    .map_err(join_error)?
}

async fn rewrite_outbox_item(path: PathBuf, item: NotificationOutboxItem) -> bool {
    tokio::task::spawn_blocking(move || write_json_atomic(&path, &item).is_ok())
        .await
        .unwrap_or(false)
}

async fn remove_outbox_file(path: PathBuf) -> bool {
    tokio::task::spawn_blocking(move || match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    })
    .await
    .unwrap_or(false)
}

async fn move_failed_outbox_item(
    root: &Path,
    source: PathBuf,
    item: NotificationOutboxItem,
) -> bool {
    let Some(file_name) = source.file_name().map(|name| name.to_owned()) else {
        return false;
    };
    let destination = failed_dir(root).join(file_name);
    tokio::task::spawn_blocking(move || {
        write_json_atomic(&destination, &item)?;
        match std::fs::remove_file(&source) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    })
    .await
    .is_ok_and(|result: std::io::Result<()>| result.is_ok())
}

async fn move_invalid_outbox_file(root: &Path, source: PathBuf) -> bool {
    let Some(file_name) = source.file_name().map(|name| name.to_owned()) else {
        return false;
    };
    let destination = failed_dir(root).join(file_name);
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::rename(&source, &destination) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    })
    .await
    .is_ok_and(|result: std::io::Result<()>| result.is_ok())
}

fn backoff_ms(attempts: u32, options: NotificationOutboxDrainOptions) -> u64 {
    let shift = attempts.saturating_sub(1).min(10);
    let secs = 5_u64
        .saturating_mul(1_u64 << shift)
        .min(options.max_backoff_secs.max(1));
    secs.saturating_mul(1_000)
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn join_error(err: tokio::task::JoinError) -> NotificationOutboxError {
    NotificationOutboxError::Io(std::io::Error::other(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_types::{
        NotificationEvent, NotificationEventKind, NotificationSeverity,
        NotificationSubscriptionConfig, TelegramNotificationConfig,
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

    #[test]
    fn enqueue_notification_writes_atomic_outbox_item() {
        let root = tempfile::tempdir().expect("tempdir");

        let path = enqueue_notification(root.path(), task_event()).expect("enqueue");

        assert!(path.starts_with(outbox_dir(root.path())));
        let item: NotificationOutboxItem =
            serde_json::from_str(&std::fs::read_to_string(path).expect("read item"))
                .expect("parse item");
        assert_eq!(item.event.kind, NotificationEventKind::TaskCompleted);
        assert_eq!(item.attempts, 0);
    }

    #[tokio::test]
    async fn drain_discards_currently_unsubscribed_events() {
        let root = tempfile::tempdir().expect("tempdir");
        enqueue_notification(root.path(), task_event()).expect("enqueue");

        let report = drain_notification_outbox(
            ExternalNotificationsConfig::default(),
            root.path().to_path_buf(),
            NotificationOutboxDrainOptions::default(),
        )
        .await;

        assert_eq!(report.scanned, 1);
        assert_eq!(report.discarded, 1);
        assert!(pending_outbox_files(root.path(), 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn drain_retries_provider_failures_without_losing_item() {
        let root = tempfile::tempdir().expect("tempdir");
        enqueue_notification(root.path(), task_event()).expect("enqueue");
        let missing_suffix = uuid::Uuid::new_v4();
        let config = ExternalNotificationsConfig {
            enabled: true,
            providers: brehon_types::NotificationProvidersConfig {
                telegram: TelegramNotificationConfig {
                    enabled: true,
                    bot_token_env: format!("BREHON_TEST_MISSING_TOKEN_{missing_suffix}"),
                    chat_id_env: format!("BREHON_TEST_MISSING_CHAT_{missing_suffix}"),
                    send_timeout_secs: 1,
                },
            },
            subscriptions: vec![NotificationSubscriptionConfig {
                provider: NotificationProviderKind::Telegram,
                events: vec![NotificationEventKind::TaskCompleted],
            }],
        };

        let report = drain_notification_outbox(
            config,
            root.path().to_path_buf(),
            NotificationOutboxDrainOptions {
                max_scan: 10,
                max_deliveries: 10,
                max_attempts: 3,
                max_backoff_secs: 60,
            },
        )
        .await;

        assert_eq!(report.attempted, 1);
        assert_eq!(report.retry_scheduled, 1);
        let files = pending_outbox_files(root.path(), 10).await.unwrap();
        assert_eq!(files.len(), 1);
        let item: NotificationOutboxItem =
            serde_json::from_str(&std::fs::read_to_string(&files[0]).expect("read item"))
                .expect("parse item");
        assert_eq!(item.attempts, 1);
        assert!(item.last_error.is_some());
        assert!(item.next_attempt_at_ms > item.queued_at_ms);
    }
}

//! External operator notification helpers for the run loop.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use brehon_types::{NotificationEvent, NotificationEventKind, NotificationSeverity};

use super::event_loop::EventLoopCtx;
use super::research::ProjectConfigLoader;
use super::types::DashboardData;

pub(crate) type NotificationOutboxDrainTask =
    tokio::task::JoinHandle<brehon_notify::NotificationOutboxDrainReport>;

pub(crate) struct NotificationOutboxState {
    pub last_drain: Instant,
    pub interval: Duration,
    pub pending: Option<NotificationOutboxDrainTask>,
}

impl NotificationOutboxState {
    pub(crate) fn live() -> Self {
        Self::new(Duration::from_secs(2))
    }

    pub(crate) fn new(interval: Duration) -> Self {
        let now = Instant::now();
        Self {
            last_drain: now.checked_sub(interval).unwrap_or(now),
            interval,
            pending: None,
        }
    }

    pub(crate) fn idle(now: Instant, interval: Duration) -> Self {
        Self {
            last_drain: now,
            interval,
            pending: None,
        }
    }
}

pub(crate) fn notify_from_ctx(ctx: &EventLoopCtx, event: NotificationEvent) {
    notify_from_parts(
        &ctx.rt,
        &ctx.dashboard_data,
        &ctx.project_config_loader,
        event,
    );
}

pub(crate) fn notify_from_parts(
    runtime: &tokio::runtime::Handle,
    dashboard_data: &Arc<parking_lot::Mutex<DashboardData>>,
    _project_config_loader: &ProjectConfigLoader,
    event: NotificationEvent,
) {
    let Some(root) = notification_brehon_root(dashboard_data) else {
        return;
    };
    runtime.spawn_blocking(move || {
        if let Err(err) = brehon_notify::enqueue_notification(&root, event) {
            tracing::warn!(error = %err, "external notification enqueue failed");
        }
    });
}

pub(crate) fn notify_now_from_parts(
    runtime: &tokio::runtime::Handle,
    dashboard_data: &Arc<parking_lot::Mutex<DashboardData>>,
    project_config_loader: &ProjectConfigLoader,
    event: NotificationEvent,
) {
    let Some(root) = notification_brehon_root(dashboard_data) else {
        return;
    };
    if let Err(err) = brehon_notify::enqueue_notification(&root, event) {
        tracing::warn!(error = %err, "external notification enqueue failed");
        return;
    }
    let Some(config) = load_notification_config(dashboard_data, project_config_loader) else {
        return;
    };
    let report = runtime.block_on(brehon_notify::drain_notification_outbox(
        config.notifications,
        root,
        brehon_notify::NotificationOutboxDrainOptions {
            max_scan: 512,
            max_deliveries: 64,
            ..brehon_notify::NotificationOutboxDrainOptions::default()
        },
    ));
    log_outbox_drain_report(report);
}

pub(crate) fn spawn_outbox_drain(
    runtime: &tokio::runtime::Handle,
    dashboard_data: &Arc<parking_lot::Mutex<DashboardData>>,
    project_config_loader: &ProjectConfigLoader,
) -> Option<NotificationOutboxDrainTask> {
    let root = notification_brehon_root(dashboard_data)?;
    let loader = Arc::clone(project_config_loader);
    Some(runtime.spawn(async move {
        let config_root = project_root_for_config(&root);
        let config = match tokio::task::spawn_blocking(move || loader(&config_root)).await {
            Ok(Some(config)) => config,
            Ok(None) => return brehon_notify::NotificationOutboxDrainReport::default(),
            Err(err) => {
                tracing::warn!(error = %err, "external notification drain skipped: config load task failed");
                return brehon_notify::NotificationOutboxDrainReport {
                    errors: 1,
                    ..brehon_notify::NotificationOutboxDrainReport::default()
                };
            }
        };
        brehon_notify::drain_notification_outbox(
            config.notifications,
            root,
            brehon_notify::NotificationOutboxDrainOptions::default(),
        )
        .await
    }))
}

pub(crate) fn log_outbox_drain_report(report: brehon_notify::NotificationOutboxDrainReport) {
    if report.errors > 0 || report.failed > 0 || report.invalid > 0 {
        tracing::warn!(
            scanned = report.scanned,
            attempted = report.attempted,
            delivered = report.delivered,
            retry_scheduled = report.retry_scheduled,
            failed = report.failed,
            invalid = report.invalid,
            errors = report.errors,
            "external notification outbox drain had failures"
        );
    } else if report.delivered > 0 || report.discarded > 0 || report.retry_scheduled > 0 {
        tracing::debug!(
            scanned = report.scanned,
            attempted = report.attempted,
            delivered = report.delivered,
            discarded = report.discarded,
            deferred = report.deferred,
            retry_scheduled = report.retry_scheduled,
            "external notification outbox drained"
        );
    }
}

pub(crate) fn service_outbox(ctx: &mut EventLoopCtx) {
    if ctx
        .notification_outbox
        .pending
        .as_ref()
        .is_some_and(tokio::task::JoinHandle::is_finished)
    {
        if let Some(handle) = ctx.notification_outbox.pending.take() {
            match ctx.rt.block_on(handle) {
                Ok(report) => log_outbox_drain_report(report),
                Err(err) => tracing::warn!(
                    error = %err,
                    "external notification outbox drain task failed"
                ),
            }
        }
    }

    if ctx.notification_outbox.pending.is_none()
        && ctx.notification_outbox.last_drain.elapsed() >= ctx.notification_outbox.interval
    {
        ctx.notification_outbox.last_drain = Instant::now();
        ctx.notification_outbox.pending =
            spawn_outbox_drain(&ctx.rt, &ctx.dashboard_data, &ctx.project_config_loader);
    }
}

pub(crate) fn finish_pending_outbox_drain(ctx: &mut EventLoopCtx) {
    if let Some(handle) = ctx.notification_outbox.pending.take() {
        match ctx.rt.block_on(handle) {
            Ok(report) => log_outbox_drain_report(report),
            Err(err) => tracing::warn!(
                error = %err,
                "external notification outbox drain task failed"
            ),
        }
    }
}

pub(crate) fn run_started_event(
    session_name: Option<&str>,
    worker_count: usize,
    reviewer_count: usize,
    advisor_count: usize,
    research_count: usize,
) -> NotificationEvent {
    let mut event = NotificationEvent::new(
        NotificationEventKind::RunStarted,
        NotificationSeverity::Info,
        "Run started",
        "Brehon started an operator run.",
    )
    .field("workers", worker_count.to_string())
    .field("reviewers", reviewer_count.to_string())
    .field("advisors", advisor_count.to_string())
    .field("researchers", research_count.to_string());
    if let Some(session) = session_name {
        event = event.field("session", session);
    }
    event
}

pub(crate) fn run_shutdown_event(
    session_name: Option<&str>,
    elapsed_secs: u64,
) -> NotificationEvent {
    let mut event = NotificationEvent::new(
        NotificationEventKind::RunShutdown,
        NotificationSeverity::Info,
        "Run shutdown",
        "Brehon shut down the operator run.",
    )
    .field("elapsed_secs", elapsed_secs.to_string());
    if let Some(session) = session_name {
        event = event.field("session", session);
    }
    event
}

pub(crate) fn budget_warning_event(reason: &str) -> NotificationEvent {
    NotificationEvent::new(
        NotificationEventKind::BudgetWarning,
        NotificationSeverity::Warning,
        "Budget warning",
        "Brehon budget usage reached a configured warning threshold.",
    )
    .field("reason", reason)
}

pub(crate) fn budget_kill_switch_event(reason: &str) -> NotificationEvent {
    NotificationEvent::new(
        NotificationEventKind::BudgetKillSwitch,
        NotificationSeverity::Critical,
        "Budget kill-switch fired",
        "Brehon refused new spend and is tearing down in-flight agents.",
    )
    .field("reason", reason)
}

pub(crate) fn crash_detected_event(pane_id: &str, reason: &str) -> NotificationEvent {
    NotificationEvent::new(
        NotificationEventKind::RunCrashDetected,
        NotificationSeverity::Critical,
        "Runtime crash detected",
        "Brehon detected a supervisor runtime failure and attempted recovery.",
    )
    .field("pane", pane_id)
    .field("reason", reason)
}

pub(crate) fn run_health_warning_event(
    elapsed_ms: u128,
    mux_output_bytes: usize,
    mux_events: usize,
    pending_prompts: usize,
) -> NotificationEvent {
    NotificationEvent::new(
        NotificationEventKind::RunHealthWarning,
        NotificationSeverity::Warning,
        "Run health warning",
        "Brehon observed a slow TUI event-loop tick.",
    )
    .field("elapsed_ms", elapsed_ms.to_string())
    .field("mux_output_bytes", mux_output_bytes.to_string())
    .field("mux_events", mux_events.to_string())
    .field("pending_prompts", pending_prompts.to_string())
}

fn load_notification_config(
    dashboard_data: &Arc<parking_lot::Mutex<DashboardData>>,
    project_config_loader: &ProjectConfigLoader,
) -> Option<brehon_types::BrehonConfig> {
    let brehon_root = notification_brehon_root(dashboard_data)?;
    let project_root = project_root_for_config(&brehon_root);
    project_config_loader(&project_root)
}

fn notification_brehon_root(
    dashboard_data: &Arc<parking_lot::Mutex<DashboardData>>,
) -> Option<PathBuf> {
    dashboard_data.lock().brehon_root.clone()
}

fn project_root_for_config(brehon_root: &Path) -> PathBuf {
    if brehon_root.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
        if let Some(parent) = brehon_root.parent() {
            return parent.to_path_buf();
        }
    }
    brehon_root.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_kill_switch_event_is_critical_and_relevant() {
        let event = budget_kill_switch_event("token limit reached");
        assert_eq!(event.kind, NotificationEventKind::BudgetKillSwitch);
        assert_eq!(event.severity, NotificationSeverity::Critical);
        assert_eq!(
            event.fields.get("reason").map(String::as_str),
            Some("token limit reached")
        );
    }

    #[test]
    fn run_health_warning_event_includes_tick_context() {
        let event = run_health_warning_event(1_250, 4096, 12, 3);

        assert_eq!(event.kind, NotificationEventKind::RunHealthWarning);
        assert_eq!(event.severity, NotificationSeverity::Warning);
        assert_eq!(
            event.fields.get("elapsed_ms").map(String::as_str),
            Some("1250")
        );
        assert_eq!(
            event.fields.get("pending_prompts").map(String::as_str),
            Some("3")
        );
    }
}

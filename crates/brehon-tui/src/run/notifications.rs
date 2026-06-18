//! External operator notification helpers for the run loop.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use brehon_types::{NotificationEvent, NotificationEventKind, NotificationSeverity};

use super::event_loop::EventLoopCtx;
use super::research::ProjectConfigLoader;
use super::types::DashboardData;

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
    project_config_loader: &ProjectConfigLoader,
    event: NotificationEvent,
) {
    let Some(config) = load_notification_config(dashboard_data, project_config_loader) else {
        return;
    };
    let _spawned = brehon_notify::spawn_notification(runtime, config.notifications, event);
}

pub(crate) fn notify_now_from_parts(
    runtime: &tokio::runtime::Handle,
    dashboard_data: &Arc<parking_lot::Mutex<DashboardData>>,
    project_config_loader: &ProjectConfigLoader,
    event: NotificationEvent,
) {
    let Some(config) = load_notification_config(dashboard_data, project_config_loader) else {
        return;
    };
    if !config.notifications.has_subscriber_for(event.kind) {
        return;
    }
    runtime.block_on(async move {
        if let Err(err) = brehon_notify::deliver_notification(config.notifications, event).await {
            tracing::warn!(error = %err, "external notification delivery failed");
        }
    });
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

fn load_notification_config(
    dashboard_data: &Arc<parking_lot::Mutex<DashboardData>>,
    project_config_loader: &ProjectConfigLoader,
) -> Option<brehon_types::BrehonConfig> {
    let brehon_root = dashboard_data.lock().brehon_root.clone()?;
    let project_root = project_root_for_config(&brehon_root);
    project_config_loader(&project_root)
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
}

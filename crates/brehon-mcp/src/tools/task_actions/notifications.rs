//! External notifications for task-action state changes.

use serde_json::{Map, Value};

use brehon_types::{NotificationEvent, NotificationEventKind, NotificationSeverity};

use super::paths::brehon_root_dir;

pub(super) fn publish_task_completed(task_id: &str, task: &Map<String, Value>, status: &str) {
    publish(
        task_event(
            NotificationEventKind::TaskCompleted,
            NotificationSeverity::Info,
            "Task completed",
            format!("Task {task_id} is ready for review."),
            task_id,
            task,
        )
        .field("status", status),
    );
}

pub(super) fn publish_task_closed(task_id: &str, task: &Map<String, Value>, status: &str) {
    let task_type = task_type(task);
    if task_type == "epic" {
        publish(
            task_event(
                NotificationEventKind::EpicCompleted,
                NotificationSeverity::Info,
                "Epic completed",
                format!("Epic {task_id} reached terminal status {status}."),
                task_id,
                task,
            )
            .field("status", status),
        );
    } else {
        publish(
            task_event(
                NotificationEventKind::TaskClosed,
                NotificationSeverity::Info,
                "Task closed",
                format!("Task {task_id} reached terminal status {status}."),
                task_id,
                task,
            )
            .field("status", status),
        );
    }
}

pub(super) fn publish_task_blocked(task_id: &str, task: &Map<String, Value>) {
    publish(
        task_event(
            NotificationEventKind::TaskBlocked,
            NotificationSeverity::Warning,
            "Task blocked",
            format!("Task {task_id} is blocked."),
            task_id,
            task,
        )
        .field("blockers", value_str(task, "blockers")),
    );
}

pub(super) fn publish_review_outcome(task_id: &str, task: &Map<String, Value>, status: &str) {
    match status {
        "approved" => publish(task_event(
            NotificationEventKind::ReviewApproved,
            NotificationSeverity::Info,
            "Review approved",
            format!("Review approved task {task_id}."),
            task_id,
            task,
        )),
        "changes_requested" => publish(task_event(
            NotificationEventKind::ReviewRejected,
            NotificationSeverity::Warning,
            "Review changes requested",
            format!("Review requested changes for task {task_id}."),
            task_id,
            task,
        )),
        _ => {}
    }
}

pub(super) fn publish_integration_started(
    task_id: &str,
    task: &Map<String, Value>,
    merge_target: &str,
) {
    publish(
        task_event(
            NotificationEventKind::IntegrationStarted,
            NotificationSeverity::Info,
            "Integration started",
            format!("Brehon started integrating task {task_id}."),
            task_id,
            task,
        )
        .field("merge_target", merge_target),
    );
}

pub(super) fn publish_integration_completed(
    task_id: &str,
    task: &Map<String, Value>,
    merge_target: &str,
    merged_commit: &str,
) {
    publish(
        task_event(
            NotificationEventKind::IntegrationCompleted,
            NotificationSeverity::Info,
            "Integration completed",
            format!("Brehon integrated task {task_id}."),
            task_id,
            task,
        )
        .field("merge_target", merge_target)
        .field("merged_commit", merged_commit),
    );
}

pub(super) fn publish_integration_failed(
    task_id: &str,
    task: &Map<String, Value>,
    merge_target: &str,
    reason: &str,
) {
    publish(
        task_event(
            NotificationEventKind::IntegrationFailed,
            NotificationSeverity::Critical,
            "Integration failed",
            format!("Brehon could not integrate task {task_id}."),
            task_id,
            task,
        )
        .field("merge_target", merge_target)
        .field("reason", reason),
    );
}

fn task_event(
    kind: NotificationEventKind,
    severity: NotificationSeverity,
    title: impl Into<String>,
    message: impl Into<String>,
    task_id: &str,
    task: &Map<String, Value>,
) -> NotificationEvent {
    NotificationEvent::new(kind, severity, title, message)
        .field("task_id", task_id)
        .field("task_type", task_type(task))
        .field("title", value_str(task, "title"))
        .field("assignee", value_str(task, "assignee"))
        .field("parent_id", value_str(task, "parent_id"))
        .field("merge_target", value_str(task, "merge_target"))
        .field("merged_branch", value_str(task, "merged_branch"))
        .field("merged_commit", value_str(task, "merged_commit"))
}

fn task_type(task: &Map<String, Value>) -> &str {
    task.get("task_type")
        .and_then(Value::as_str)
        .unwrap_or("task")
}

fn value_str(task: &Map<String, Value>, key: &str) -> String {
    task.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn publish(event: NotificationEvent) {
    let Some(root) = brehon_root_dir() else {
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("external notification skipped: no active tokio runtime");
        return;
    };
    handle.spawn_blocking(move || {
        if let Err(err) = brehon_notify::enqueue_notification(&root, event) {
            tracing::warn!(error = %err, "external notification enqueue failed");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ScopedEnv, TEST_ENV_LOCK};
    use std::time::{Duration, Instant};

    #[test]
    fn task_event_includes_context_fields() {
        let task = serde_json::json!({
            "task_id": "T-1",
            "task_type": "task",
            "title": "Wire notifications",
            "assignee": "worker-1",
            "parent_id": "E-1",
            "merge_target": "epic/notify"
        });
        let task = task.as_object().expect("object");

        let event = task_event(
            NotificationEventKind::TaskCompleted,
            NotificationSeverity::Info,
            "Task completed",
            "Task T-1 is ready for review.",
            "T-1",
            task,
        );

        assert_eq!(event.fields.get("task_id").map(String::as_str), Some("T-1"));
        assert_eq!(
            event.fields.get("parent_id").map(String::as_str),
            Some("E-1")
        );
        assert_eq!(
            event.fields.get("merge_target").map(String::as_str),
            Some("epic/notify")
        );
    }

    #[test]
    fn publish_enqueues_outbox_item_under_brehon_root() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let root = tempfile::tempdir().expect("tempdir");
        let _env = ScopedEnv::set(&[("BREHON_ROOT", root.path().to_str().expect("utf8 path"))]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            publish(NotificationEvent::new(
                NotificationEventKind::TaskCompleted,
                NotificationSeverity::Info,
                "Task completed",
                "Task T-1 is ready for review.",
            ));
            tokio::time::sleep(Duration::from_millis(25)).await;
        });

        let outbox = root.path().join("runtime/notifications/outbox");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let count = std::fs::read_dir(&outbox)
                .map(|entries| entries.filter_map(Result::ok).count())
                .unwrap_or(0);
            if count == 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for notification outbox item"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

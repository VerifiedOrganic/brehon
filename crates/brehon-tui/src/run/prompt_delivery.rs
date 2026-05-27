//! Prompt queue delivery, reviewer reset, and worker recycle logic
//! extracted from the main event loop.

use std::path::Path;
use std::time::Instant;

use brehon_mux::{
    AsyncGatewayPromptDispatch, PaneKind, PaneState, PromptDeliveryAttempt, SessionScopedQueue,
};
use brehon_types::task::normalize_task_status;
use brehon_types::{
    PromptDeliveryMode, RuntimeCommand, RuntimeCommandKind, RuntimeCommandStatus,
    RuntimeCommandTarget, RuntimePaneState, RuntimePolicyContext,
};

use super::dashboard::read_task_files;
use super::event_loop::{queue_runtime_command, EventLoopCtx, PendingRuntimeCommandEffect};
use super::gateway_prompts::AsyncQueuedGatewayPromptDeliveryTask;
use super::helpers::pane_needs_post_spawn_prompt;
use super::helpers::{
    write_prompt_delivery_ack, write_reviewer_reset_ack, write_worker_recycle_ack,
    ReviewerResetEntry, WorkerRecycleEntry,
};
use super::recovery::{
    agent_health_marker_reason, agent_is_quarantined_for_run, clear_agent_health_marker,
    clear_prompt_retry_meta, dead_letter_prompt_for_session, extract_consolidated_report_identity,
    prompt_retry_not_due, push_dashboard_event, queued_prompt_matches_session,
    queued_prompt_retry_delay, read_queued_prompt, record_prompt_retry_deferral,
    record_prompt_retry_failure, rewrite_stale_consolidated_report,
    runtime_prompt_queue_sweep_dirs, should_dead_letter_prompt_after_failure,
    should_drop_stale_review_prompt, QueuedPromptPayload,
};
use super::self_improvement::{
    build_reviewer_reset_startup_prompt, build_supervisor_reset_startup_prompt,
    build_worker_recycle_startup_prompt,
};
use super::types::task_is_terminal;

const TERMINAL_HOST_STARTUP_PROMPT_DELAY: std::time::Duration = std::time::Duration::from_secs(5);

fn runtime_command_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn host_prompt_policy_context(ctx: &EventLoopCtx, target: &str) -> RuntimePolicyContext {
    let pane_state =
        ctx.mux
            .get(target)
            .and_then(|pane| pane.pane_state())
            .map(|state| match state {
                brehon_mux::PaneState::Ready { .. } => RuntimePaneState::Ready,
                brehon_mux::PaneState::Busy { .. } => RuntimePaneState::Busy,
                brehon_mux::PaneState::Blocked { .. } => RuntimePaneState::Blocked,
                brehon_mux::PaneState::Dead { .. } => RuntimePaneState::Dead,
            });
    RuntimePolicyContext {
        pane_state,
        ..RuntimePolicyContext::default()
    }
}

fn is_role_alias_prompt_target(target: &str) -> bool {
    matches!(
        target.trim().to_ascii_lowercase().as_str(),
        "worker"
            | "assignee"
            | "assigned-worker"
            | "assigned_worker"
            | "task-assignee"
            | "task_assignee"
    )
}

fn resolve_role_alias_prompt_target(
    brehon_root: &Path,
    target: &str,
    prompt_text: &str,
) -> Result<Option<String>, &'static str> {
    if !is_role_alias_prompt_target(target) {
        return Ok(Some(target.to_string()));
    }

    let Some((task_id, _, _)) = extract_consolidated_report_identity(prompt_text) else {
        return Err("role alias prompt target has no consolidated review task identity");
    };
    let task_path = brehon_root
        .join("runtime")
        .join("tasks")
        .join(format!("{task_id}.json"));
    let Ok(task_content) = std::fs::read_to_string(&task_path) else {
        return Err("role alias prompt target references a missing task");
    };
    let Ok(task) = serde_json::from_str::<serde_json::Value>(&task_content) else {
        return Err("role alias prompt target references an unreadable task");
    };
    let status = task
        .get("status")
        .and_then(|value| value.as_str())
        .unwrap_or("pending");
    if normalize_task_status(status).is_some_and(|status| matches!(status, "merged" | "closed")) {
        return Ok(None);
    }
    let Some(assignee) = task
        .get("assignee")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|assignee| !assignee.is_empty())
    else {
        return Err("role alias prompt target references a task with no assignee");
    };
    if is_role_alias_prompt_target(assignee) {
        return Err("role alias prompt target resolved to another role alias");
    }
    Ok(Some(assignee.to_string()))
}

fn host_runtime_target(ctx: &EventLoopCtx, target: &str) -> RuntimeCommandTarget {
    RuntimeCommandTarget {
        session_id: ctx
            .runtime_session_name
            .as_deref()
            .unwrap_or("default")
            .to_string(),
        pane_id: Some(target.to_string()),
        generation: ctx.mux.get(target).map(|pane| pane.current_generation().0),
    }
}

fn host_prompt_failure(
    ctx: &EventLoopCtx,
    brehon_root: &std::path::Path,
    path: &std::path::Path,
    target: &str,
    from: Option<&str>,
    prompt_text: &str,
    err_text: &str,
    failure_kind: &str,
) {
    if should_dead_letter_prompt_after_failure(prompt_text, err_text) {
        dead_letter_prompt_for_session(
            brehon_root,
            ctx.runtime_session_name.as_deref(),
            path,
            target,
            from,
            prompt_text,
            err_text,
            failure_kind,
        );
        push_dashboard_event(
            &ctx.dashboard_data,
            format!(
                "dead-lettered queued prompt for {target} after terminal-host delivery failure"
            ),
        );
        tracing::warn!(
            target = %target,
            error = %err_text,
            "Dead-lettered prompt-queue message after terminal-host delivery failure"
        );
    } else {
        let (attempts, next_retry_at) = record_prompt_retry_failure(path, err_text);
        tracing::warn!(
            target = %target,
            error = %err_text,
            attempts,
            next_retry_at = %next_retry_at.to_rfc3339(),
            "Terminal-host prompt delivery failed; backing off retry"
        );
    }
}

fn route_terminal_host_command(
    ctx: &EventLoopCtx,
    target: &str,
    kind: RuntimeCommandKind,
) -> Result<brehon_types::RuntimeCommandResult, String> {
    let Some(router) = ctx.runtime_command_router.clone() else {
        return Err("runtime command router unavailable".to_string());
    };
    let command = RuntimeCommand {
        command_id: format!("terminal-host-prompt-{}", uuid::Uuid::new_v4()),
        target: host_runtime_target(ctx, target),
        issued_at_ms: runtime_command_timestamp_ms(),
        kind,
    };
    ctx.rt
        .block_on(router.route_command(command, host_prompt_policy_context(ctx, target)))
        .map_err(|err| err.to_string())
}

fn host_prompt_route_applied(
    result: &brehon_types::RuntimeCommandResult,
    operation: &str,
) -> Result<(), String> {
    if result.status == RuntimeCommandStatus::Applied {
        return Ok(());
    }
    Err(format!(
        "{operation} returned {:?}: {}",
        result.status,
        result.message.clone().unwrap_or_default()
    ))
}

pub(super) fn reset_terminal_host_pane(
    ctx: &EventLoopCtx,
    target: &str,
    reason: impl Into<String>,
) -> Result<(), String> {
    route_terminal_host_command(
        ctx,
        target,
        RuntimeCommandKind::ResetPane {
            reason: reason.into(),
        },
    )
    .and_then(|result| host_prompt_route_applied(&result, "terminal-host reset"))
}

pub(super) fn recycle_terminal_host_pane(
    ctx: &EventLoopCtx,
    target: &str,
    reason: impl Into<String>,
) -> Result<(), String> {
    route_terminal_host_command(
        ctx,
        target,
        RuntimeCommandKind::RecyclePane {
            reason: reason.into(),
        },
    )
    .and_then(|result| host_prompt_route_applied(&result, "terminal-host recycle"))
}

pub(super) fn enqueue_terminal_host_startup_prompt(
    ctx: &EventLoopCtx,
    target: &str,
    prompt: String,
    reason: &str,
) -> Result<(), String> {
    let brehon_root = ctx
        .dashboard_data
        .lock()
        .unwrap()
        .brehon_root
        .clone()
        .ok_or_else(|| "brehon root unavailable for terminal-host startup prompt".to_string())?;
    let session_name = ctx
        .runtime_session_name
        .as_deref()
        .unwrap_or("_legacy")
        .to_string();
    let prompt_queue_dir = brehon_root
        .join("runtime")
        .join("prompt-queue")
        .join(&session_name);
    std::fs::create_dir_all(&prompt_queue_dir).map_err(|err| err.to_string())?;

    let prompt_id = uuid::Uuid::new_v4().to_string();
    let file_name = format!(
        "{:020}-{}.prompt",
        chrono::Utc::now().timestamp_millis(),
        prompt_id
    );
    let final_path = prompt_queue_dir.join(&file_name);
    let temp_path = prompt_queue_dir.join(format!(".{file_name}.tmp"));
    let payload = serde_json::json!({
        "target": target,
        "from": brehon_mux::teams::DIRECTOR_AGENT_NAME,
        "message": prompt,
        "session_name": session_name,
        "prompt_id": prompt_id,
    });
    let encoded = serde_json::to_string(&payload).map_err(|err| err.to_string())?;
    std::fs::write(&temp_path, encoded).map_err(|err| err.to_string())?;
    std::fs::rename(&temp_path, &final_path).map_err(|err| err.to_string())?;
    record_prompt_retry_deferral(&final_path, TERMINAL_HOST_STARTUP_PROMPT_DELAY, reason);
    Ok(())
}

fn pane_uses_teams_delivery(ctx: &EventLoopCtx, target: &str) -> bool {
    ctx.mux.teams().is_some()
        && ctx
            .mux
            .get(target)
            .is_some_and(|pane| pane.cli_type().capabilities().supports_teams)
}

fn record_prompt_deferral_and_recover(
    ctx: &mut EventLoopCtx,
    path: &std::path::Path,
    target: &str,
    retry_after: std::time::Duration,
    reason: &str,
) -> chrono::DateTime<chrono::Utc> {
    let next_retry_at = record_prompt_retry_deferral(path, retry_after, reason);
    super::stall_handling::recover_stale_deferred_prompt_delivery(
        ctx,
        target,
        path,
        Instant::now(),
    );
    next_retry_at
}

fn deliver_queued_prompt_via_terminal_host(
    ctx: &mut EventLoopCtx,
    brehon_root: &std::path::Path,
    path: &std::path::Path,
    target: &str,
    from: Option<&str>,
    prompt_text: &str,
    prompt_id: Option<&str>,
) -> bool {
    if !ctx.runtime_agent_factory_host_owned || ctx.mux.get(target).is_none() {
        return false;
    }

    if pane_uses_teams_delivery(ctx, target) {
        match ctx
            .rt
            .block_on(ctx.mux.attempt_prompt_delivery(target, prompt_text, from))
        {
            Ok(PromptDeliveryAttempt::Delivered { .. }) => {
                let nudge = RuntimeCommandKind::SendTerminalInput {
                    bytes: b"\r".to_vec(),
                };
                match route_terminal_host_command(ctx, target, nudge).and_then(|result| {
                    host_prompt_route_applied(&result, "terminal-host inbox nudge")
                }) {
                    Ok(()) => {
                        let _ = ctx.mux.mark_terminal_host_inbox_nudge_dispatched(target);
                        ctx.last_activity.insert(target.to_string(), Instant::now());
                        clear_prompt_retry_meta(path);
                        if let Some(id) = prompt_id {
                            if let Err(err) = write_prompt_delivery_ack(
                                brehon_root,
                                id,
                                target,
                                "terminal_host_teams",
                            ) {
                                tracing::warn!(
                                    target = %target,
                                    prompt_id = %id,
                                    error = %err,
                                    "Failed to persist prompt delivery ack after terminal-host Teams delivery"
                                );
                            }
                        }
                        tracing::info!(
                            target = %target,
                            "Delivered queued prompt via Teams inbox and terminal-host nudge"
                        );
                        let _ = std::fs::remove_file(path);
                    }
                    Err(err_text) => host_prompt_failure(
                        ctx,
                        brehon_root,
                        path,
                        target,
                        from,
                        prompt_text,
                        &err_text,
                        "terminal-host inbox nudge failure",
                    ),
                }
            }
            Ok(PromptDeliveryAttempt::Queued {
                prompt_id,
                ahead_of,
            }) => {
                let retry_after = queued_prompt_retry_delay(ahead_of);
                let next_retry_at = record_prompt_deferral_and_recover(
                    ctx,
                    path,
                    target,
                    retry_after,
                    "terminal-host Teams prompt not ready",
                );
                tracing::info!(
                    target = %target,
                    prompt_id = %prompt_id,
                    ahead_of,
                    next_retry_at = %next_retry_at.to_rfc3339(),
                    retry_after_ms = %retry_after.as_millis(),
                    "Queued terminal-host Teams prompt before nudge"
                );
            }
            Ok(PromptDeliveryAttempt::AlreadyPresent {
                prompt_id,
                position,
            }) => {
                let retry_after = queued_prompt_retry_delay(position.retry_ahead_of());
                let next_retry_at = record_prompt_deferral_and_recover(
                    ctx,
                    path,
                    target,
                    retry_after,
                    "terminal-host Teams prompt already queued",
                );
                tracing::info!(
                    target = %target,
                    prompt_id = %prompt_id,
                    position = %position,
                    next_retry_at = %next_retry_at.to_rfc3339(),
                    retry_after_ms = %retry_after.as_millis(),
                    "Queued prompt already present in Teams transport"
                );
            }
            Ok(PromptDeliveryAttempt::Rejected { reason }) => {
                let err_text = format!("terminal-host Teams prompt rejected: {reason:?}");
                host_prompt_failure(
                    ctx,
                    brehon_root,
                    path,
                    target,
                    from,
                    prompt_text,
                    &err_text,
                    "terminal-host Teams prompt rejection",
                );
            }
            Err(err) => {
                let err_text = err.to_string();
                host_prompt_failure(
                    ctx,
                    brehon_root,
                    path,
                    target,
                    from,
                    prompt_text,
                    &err_text,
                    "terminal-host Teams prompt failure",
                );
            }
        }
        return true;
    }

    let delivery_prompt_id = prompt_id
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let kind = RuntimeCommandKind::SendPrompt {
        prompt_id: delivery_prompt_id,
        text: prompt_text.to_string(),
        from: from.map(ToOwned::to_owned),
        delivery: PromptDeliveryMode::Direct,
    };
    match route_terminal_host_command(ctx, target, kind)
        .and_then(|result| host_prompt_route_applied(&result, "terminal-host prompt delivery"))
    {
        Ok(()) => {
            ctx.last_activity.insert(target.to_string(), Instant::now());
            clear_prompt_retry_meta(path);
            if let Some(id) = prompt_id {
                if let Err(err) =
                    write_prompt_delivery_ack(brehon_root, id, target, "terminal_host")
                {
                    tracing::warn!(
                        target = %target,
                        prompt_id = %id,
                        error = %err,
                        "Failed to persist prompt delivery ack after terminal-host delivery"
                    );
                }
            }
            tracing::info!(
                target = %target,
                "Delivered queued prompt via terminal-host prompt command"
            );
            let _ = std::fs::remove_file(path);
        }
        Err(err_text) => host_prompt_failure(
            ctx,
            brehon_root,
            path,
            target,
            from,
            prompt_text,
            &err_text,
            "terminal-host prompt delivery failure",
        ),
    }
    true
}

pub(super) fn dispatch_runtime_prompt(
    ctx: &mut EventLoopCtx,
    target: &str,
    prompt: String,
    from: Option<String>,
) -> bool {
    if !ctx.runtime_agent_factory_host_owned {
        if ctx.mux.get(target).is_none() {
            return false;
        }
        let kind = RuntimeCommandKind::SendPrompt {
            prompt_id: uuid::Uuid::new_v4().to_string(),
            text: prompt.clone(),
            from: from.clone(),
            delivery: PromptDeliveryMode::Enqueue,
        };
        let command = RuntimeCommand {
            command_id: format!("runtime-prompt-{}", uuid::Uuid::new_v4()),
            target: host_runtime_target(ctx, target),
            issued_at_ms: runtime_command_timestamp_ms(),
            kind,
        };
        let context = host_prompt_policy_context(ctx, target);
        if queue_runtime_command(
            ctx,
            command,
            context,
            PendingRuntimeCommandEffect::DashboardAction {
                pane_id: Some(target.to_string()),
                success_message: None,
                failure_prefix: format!("runtime prompt for {target} failed"),
                update_activity: true,
                clear_pending_self_improve: false,
            },
        )
        .is_ok()
        {
            return true;
        }

        ctx.mux
            .dispatch_deliver_prompt(&ctx.rt, target, prompt, from);
        return true;
    }
    if ctx.mux.get(target).is_none() {
        return false;
    }

    if pane_uses_teams_delivery(ctx, target) {
        match ctx.rt.block_on(
            ctx.mux
                .attempt_prompt_delivery(target, &prompt, from.as_deref()),
        ) {
            Ok(PromptDeliveryAttempt::Delivered { .. }) => {
                let nudge = RuntimeCommandKind::SendTerminalInput {
                    bytes: b"\r".to_vec(),
                };
                match route_terminal_host_command(ctx, target, nudge).and_then(|result| {
                    host_prompt_route_applied(&result, "terminal-host inbox nudge")
                }) {
                    Ok(()) => {
                        let _ = ctx.mux.mark_terminal_host_inbox_nudge_dispatched(target);
                        ctx.last_activity.insert(target.to_string(), Instant::now());
                        true
                    }
                    Err(err) => {
                        push_dashboard_event(
                            &ctx.dashboard_data,
                            format!("terminal-host inbox nudge for {target} failed: {err}"),
                        );
                        tracing::warn!(target = %target, error = %err, "Terminal-host inbox nudge failed");
                        false
                    }
                }
            }
            Ok(PromptDeliveryAttempt::Queued { ahead_of, .. }) => {
                let retry_after = queued_prompt_retry_delay(ahead_of);
                if let Err(err) = enqueue_terminal_host_startup_prompt(
                    ctx,
                    target,
                    prompt,
                    "terminal-host Teams prompt deferred",
                ) {
                    push_dashboard_event(
                        &ctx.dashboard_data,
                        format!("failed to defer terminal-host Teams prompt for {target}: {err}"),
                    );
                    return false;
                }
                tracing::info!(
                    target = %target,
                    retry_after_ms = %retry_after.as_millis(),
                    "Deferred terminal-host Teams prompt to durable queue"
                );
                true
            }
            Ok(PromptDeliveryAttempt::AlreadyPresent { .. }) => true,
            Ok(PromptDeliveryAttempt::Rejected { reason }) => {
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!("terminal-host Teams prompt for {target} rejected: {reason:?}"),
                );
                false
            }
            Err(err) => {
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!("terminal-host Teams prompt for {target} failed: {err}"),
                );
                false
            }
        }
    } else {
        let kind = RuntimeCommandKind::SendPrompt {
            prompt_id: uuid::Uuid::new_v4().to_string(),
            text: prompt,
            from: None,
            delivery: PromptDeliveryMode::Direct,
        };
        match route_terminal_host_command(ctx, target, kind)
            .and_then(|result| host_prompt_route_applied(&result, "terminal-host prompt delivery"))
        {
            Ok(()) => {
                ctx.last_activity.insert(target.to_string(), Instant::now());
                true
            }
            Err(err) => {
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!("terminal-host prompt for {target} failed: {err}"),
                );
                tracing::warn!(target = %target, error = %err, "Terminal-host prompt delivery failed");
                false
            }
        }
    }
}

fn queue_queued_prompt_delivery_via_daemon(
    ctx: &mut EventLoopCtx,
    brehon_root: &std::path::Path,
    path: &std::path::Path,
    target: &str,
    from: Option<&str>,
    prompt_text: &str,
    prompt_id: Option<&str>,
) -> bool {
    let Some(pane) = ctx.mux.get(target) else {
        return false;
    };

    if pane.is_gateway_backed() {
        return false;
    }

    let delivery_prompt_id = prompt_id
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let command = RuntimeCommand {
        command_id: format!("queued-prompt-{}", uuid::Uuid::new_v4()),
        target: host_runtime_target(ctx, target),
        issued_at_ms: runtime_command_timestamp_ms(),
        kind: RuntimeCommandKind::SendPrompt {
            prompt_id: delivery_prompt_id,
            text: prompt_text.to_string(),
            from: from.map(ToOwned::to_owned),
            delivery: PromptDeliveryMode::Attempt,
        },
    };
    let context = host_prompt_policy_context(ctx, target);
    if queue_runtime_command(
        ctx,
        command,
        context,
        PendingRuntimeCommandEffect::QueuedPromptDelivery {
            path: path.to_path_buf(),
            target: target.to_string(),
            from: from.map(ToOwned::to_owned),
            prompt_id: prompt_id.map(ToOwned::to_owned),
            prompt_text: prompt_text.to_string(),
            brehon_root: brehon_root.to_path_buf(),
            runtime_session_name: ctx.runtime_session_name.clone(),
            method: "daemon_runtime".to_string(),
        },
    )
    .is_err()
    {
        return false;
    }

    ctx.needs_redraw = true;
    true
}

pub(super) fn deliver_pending_prompts(ctx: &mut EventLoopCtx, brehon_root: &std::path::Path) {
    for pq_dir in runtime_prompt_queue_sweep_dirs(brehon_root, ctx.runtime_session_name.as_deref())
    {
        let Ok(entries) = std::fs::read_dir(&pq_dir) else {
            continue;
        };
        let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();

        for path in paths {
            let is_prompt_file = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| matches!(ext, "prompt" | "entry"));
            if !is_prompt_file {
                continue;
            }
            if ctx
                .pending_queued_gateway_prompt_deliveries
                .iter()
                .any(|task| task.path == path)
            {
                continue;
            }
            if prompt_retry_not_due(&path) {
                continue;
            }
            let queued = read_queued_prompt(&path).or_else(|| {
                let worker = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                std::fs::read_to_string(&path)
                    .ok()
                    .map(|prompt_text| QueuedPromptPayload {
                        target: worker,
                        from: None,
                        message: prompt_text,
                        session_name: None,
                        prompt_id: None,
                    })
            });

            if let Some(QueuedPromptPayload {
                target,
                from,
                message: prompt_text,
                session_name: prompt_session_name,
                prompt_id,
            }) = queued
            {
                if !queued_prompt_matches_session(
                    ctx.runtime_session_name.as_deref(),
                    prompt_session_name.as_deref(),
                ) {
                    dead_letter_prompt_for_session(
                        brehon_root,
                        ctx.runtime_session_name.as_deref(),
                        &path,
                        &target,
                        from.as_deref(),
                        &prompt_text,
                        "prompt belongs to a different Brehon runtime session",
                        "session mismatch",
                    );
                    push_dashboard_event(
                        &ctx.dashboard_data,
                        format!(
                            "dead-lettered queued prompt for {target} because it belongs to another runtime session"
                        ),
                    );
                    continue;
                }
                if should_drop_stale_review_prompt(brehon_root, &prompt_text) {
                    tracing::info!(
                        target = %target,
                        path = %path.display(),
                        "Dropping stale queued review prompt"
                    );
                    let _ = std::fs::remove_file(&path);
                    continue;
                }

                let target = match resolve_role_alias_prompt_target(
                    brehon_root,
                    &target,
                    &prompt_text,
                ) {
                    Ok(Some(resolved)) => resolved,
                    Ok(None) => {
                        tracing::info!(
                            target = %target,
                            path = %path.display(),
                            "Dropping queued prompt for terminal task with role-alias target"
                        );
                        let _ = std::fs::remove_file(&path);
                        clear_prompt_retry_meta(&path);
                        push_dashboard_event(
                            &ctx.dashboard_data,
                            format!(
                                "dropped queued prompt for {target} because the task is already terminal"
                            ),
                        );
                        continue;
                    }
                    Err(reason) => {
                        dead_letter_prompt_for_session(
                            brehon_root,
                            ctx.runtime_session_name.as_deref(),
                            &path,
                            &target,
                            from.as_deref(),
                            &prompt_text,
                            reason,
                            "unresolvable role alias prompt target",
                        );
                        push_dashboard_event(
                            &ctx.dashboard_data,
                            format!("dead-lettered queued prompt for {target}: {reason}"),
                        );
                        continue;
                    }
                };
                let prompt_text = rewrite_stale_consolidated_report(brehon_root, &prompt_text)
                    .unwrap_or(prompt_text);

                if agent_is_quarantined_for_run(brehon_root, &target) {
                    let target_is_supervisor = ctx.mux.get(&target).is_some_and(|pane| {
                        pane.kind() == &PaneKind::Supervisor
                            && !matches!(pane.pane_state(), Some(PaneState::Dead { .. }))
                    });
                    let mut quarantine_cleared = false;
                    if target_is_supervisor {
                        let marker_reason = agent_health_marker_reason(brehon_root, &target);
                        let supervisor_ready = ctx.mux.get(&target).is_some_and(|pane| {
                            matches!(pane.pane_state(), Some(PaneState::Ready { .. }))
                        });
                        if marker_reason.as_deref() == Some("prompt_blocked") && supervisor_ready {
                            clear_agent_health_marker(brehon_root, &target);
                            quarantine_cleared = true;
                            push_dashboard_event(
                                &ctx.dashboard_data,
                                format!(
                                    "cleared stale prompt-blocked marker for ready supervisor {target}"
                                ),
                            );
                        } else {
                            let next_retry_at = record_prompt_retry_deferral(
                                &path,
                                TERMINAL_HOST_STARTUP_PROMPT_DELAY,
                                "target supervisor quarantined for current run",
                            );
                            if pane_needs_post_spawn_prompt(&ctx.mux, &target) {
                                if let Some(startup_prompt) =
                                    build_supervisor_reset_startup_prompt(&ctx.mux, &target, ctx.runtime_agent_factory_host_owned)
                                {
                                    if ctx.runtime_agent_factory_host_owned {
                                        let _ = enqueue_terminal_host_startup_prompt(
                                            ctx,
                                            &target,
                                            startup_prompt,
                                            "terminal-host supervisor quarantine reset startup prompt",
                                        );
                                    } else {
                                        ctx.mux.queue_startup_prompt(&target, startup_prompt);
                                    }
                                }
                            }
                            push_dashboard_event(
                                &ctx.dashboard_data,
                                format!(
                                    "deferred queued prompt for quarantined supervisor {target}; will retry after {}",
                                    next_retry_at.to_rfc3339()
                                ),
                            );
                            continue;
                        }
                    }

                    if !quarantine_cleared {
                        dead_letter_prompt_for_session(
                            brehon_root,
                            ctx.runtime_session_name.as_deref(),
                            &path,
                            &target,
                            from.as_deref(),
                            &prompt_text,
                            "target quarantined unavailable for this run",
                            "target quarantined for current run",
                        );
                        push_dashboard_event(
                            &ctx.dashboard_data,
                            format!(
                                "dead-lettered queued prompt for {target} because the agent is quarantined for this run"
                            ),
                        );
                        continue;
                    }
                }

                if deliver_queued_prompt_via_terminal_host(
                    ctx,
                    brehon_root,
                    &path,
                    &target,
                    from.as_deref(),
                    &prompt_text,
                    prompt_id.as_deref(),
                ) {
                    continue;
                }

                if queue_queued_prompt_delivery_via_daemon(
                    ctx,
                    brehon_root,
                    &path,
                    &target,
                    from.as_deref(),
                    &prompt_text,
                    prompt_id.as_deref(),
                ) {
                    continue;
                }

                if ctx
                    .mux
                    .get(&target)
                    .is_some_and(|pane| pane.is_gateway_backed())
                {
                    match ctx.rt.block_on(ctx.mux.begin_async_gateway_prompt_delivery(
                        &ctx.rt,
                        &target,
                        &prompt_text,
                    )) {
                        Ok(AsyncGatewayPromptDispatch::Started(handle)) => {
                            tracing::info!(
                                target = %target,
                                path = %path.display(),
                                "Started queued gateway prompt delivery in background"
                            );
                            ctx.pending_queued_gateway_prompt_deliveries.push(
                                AsyncQueuedGatewayPromptDeliveryTask {
                                    path: path.clone(),
                                    target: target.clone(),
                                    from: from.clone(),
                                    prompt_id: prompt_id.clone(),
                                    prompt_text,
                                    handle,
                                    started_at: Instant::now(),
                                },
                            );
                            continue;
                        }
                        Ok(AsyncGatewayPromptDispatch::Queued {
                            prompt_id,
                            ahead_of,
                        }) => {
                            let retry_after = queued_prompt_retry_delay(ahead_of);
                            let next_retry_at = record_prompt_deferral_and_recover(
                                ctx,
                                &path,
                                &target,
                                retry_after,
                                "gateway queued prompt not ready for delivery",
                            );
                            tracing::info!(
                                target = %target,
                                prompt_id = %prompt_id,
                                ahead_of,
                                next_retry_at = %next_retry_at.to_rfc3339(),
                                retry_after_ms = %retry_after.as_millis(),
                                "Queued gateway prompt before background attempt"
                            );
                            continue;
                        }
                        Err(err) => {
                            let err_text = err.to_string();
                            if should_dead_letter_prompt_after_failure(&prompt_text, &err_text) {
                                dead_letter_prompt_for_session(
                                    brehon_root,
                                    ctx.runtime_session_name.as_deref(),
                                    &path,
                                    &target,
                                    from.as_deref(),
                                    &prompt_text,
                                    &err_text,
                                    "nonrecoverable async gateway prompt bootstrap failure",
                                );
                                push_dashboard_event(
                                    &ctx.dashboard_data,
                                    format!(
                                        "dead-lettered queued prompt for {target} after gateway bootstrap failure"
                                    ),
                                );
                                tracing::warn!(
                                    target = %target,
                                    error = %err_text,
                                    "Dead-lettered queued gateway prompt after gateway bootstrap failure"
                                );
                            } else {
                                let (attempts, next_retry_at) =
                                    record_prompt_retry_failure(&path, &err_text);
                                tracing::warn!(
                                    target = %target,
                                    error = %err_text,
                                    attempts,
                                    next_retry_at = %next_retry_at.to_rfc3339(),
                                    "Failed to prepare queued gateway prompt delivery; backing off retry"
                                );
                            }
                            continue;
                        }
                    }
                }

                match ctx.rt.block_on(ctx.mux.attempt_prompt_delivery(
                    &target,
                    &prompt_text,
                    from.as_deref(),
                )) {
                    Ok(PromptDeliveryAttempt::Delivered { .. }) => {
                        ctx.last_activity.insert(target.clone(), Instant::now());
                        clear_prompt_retry_meta(&path);
                        if let Some(id) = prompt_id.as_deref() {
                            if let Err(err) =
                                write_prompt_delivery_ack(brehon_root, id, &target, "mux_transport")
                            {
                                tracing::warn!(
                                    target = %target,
                                    prompt_id = %id,
                                    error = %err,
                                    "Failed to persist prompt delivery ack after mux transport delivery"
                                );
                            }
                        }
                        tracing::info!(
                            target = %target,
                            "Delivered queued prompt via mux transport"
                        );
                        let _ = std::fs::remove_file(&path);
                    }
                    Ok(PromptDeliveryAttempt::Queued {
                        prompt_id,
                        ahead_of,
                    }) => {
                        let retry_after = queued_prompt_retry_delay(ahead_of);
                        let next_retry_at = record_prompt_deferral_and_recover(
                            ctx,
                            &path,
                            &target,
                            retry_after,
                            "transport queued prompt delivery",
                        );
                        tracing::info!(
                            target = %target,
                            prompt_id = %prompt_id,
                            ahead_of,
                            next_retry_at = %next_retry_at.to_rfc3339(),
                            retry_after_ms = %retry_after.as_millis(),
                            "Queued prompt delivery; keeping prompt durable on disk"
                        );
                    }
                    Ok(PromptDeliveryAttempt::AlreadyPresent {
                        prompt_id,
                        position,
                    }) => {
                        let retry_after = queued_prompt_retry_delay(position.retry_ahead_of());
                        let next_retry_at = record_prompt_deferral_and_recover(
                            ctx,
                            &path,
                            &target,
                            retry_after,
                            "transport already has queued prompt delivery",
                        );
                        tracing::info!(
                            target = %target,
                            prompt_id = %prompt_id,
                            position = %position,
                            next_retry_at = %next_retry_at.to_rfc3339(),
                            retry_after_ms = %retry_after.as_millis(),
                            "Queued prompt already present in mux; keeping prompt durable on disk"
                        );
                    }
                    Ok(PromptDeliveryAttempt::Rejected { reason }) => {
                        let err_text = format!("prompt delivery rejected: {reason:?}");
                        if should_dead_letter_prompt_after_failure(&prompt_text, &err_text) {
                            dead_letter_prompt_for_session(
                                brehon_root,
                                ctx.runtime_session_name.as_deref(),
                                &path,
                                &target,
                                from.as_deref(),
                                &prompt_text,
                                &err_text,
                                "nonrecoverable prompt delivery rejection",
                            );
                            push_dashboard_event(
                                &ctx.dashboard_data,
                                format!(
                                    "dead-lettered queued prompt for {target} after delivery rejection"
                                ),
                            );
                            tracing::warn!(
                                target = %target,
                                error = %err_text,
                                "Dead-lettered prompt-queue message after delivery rejection"
                            );
                            continue;
                        }
                        let (attempts, next_retry_at) =
                            record_prompt_retry_failure(&path, &err_text);
                        tracing::warn!(
                            target = %target,
                            error = %err_text,
                            attempts,
                            next_retry_at = %next_retry_at.to_rfc3339(),
                            "Prompt delivery rejected; backing off retry"
                        );
                    }
                    Err(err) => {
                        let err_text = err.to_string();
                        if should_dead_letter_prompt_after_failure(&prompt_text, &err_text) {
                            dead_letter_prompt_for_session(
                                brehon_root,
                                ctx.runtime_session_name.as_deref(),
                                &path,
                                &target,
                                from.as_deref(),
                                &prompt_text,
                                &err_text,
                                "nonrecoverable prompt delivery failure",
                            );
                            push_dashboard_event(
                                &ctx.dashboard_data,
                                format!(
                                    "dead-lettered queued prompt for {target} after delivery failure"
                                ),
                            );
                            tracing::warn!(
                                target = %target,
                                error = %err_text,
                                "Dead-lettered prompt-queue message after nonrecoverable failure"
                            );
                            continue;
                        }
                        let (attempts, next_retry_at) =
                            record_prompt_retry_failure(&path, &err_text);
                        tracing::warn!(
                            target = %target,
                            error = %err_text,
                            attempts,
                            next_retry_at = %next_retry_at.to_rfc3339(),
                            "Failed to deliver prompt-queue message; backing off retry"
                        );
                    }
                }
            }
        }
    }

    let reviewer_reset_queue_dir = brehon_root.join("runtime").join("reviewer-reset-queue");
    let reviewer_reset_session = ctx
        .runtime_session_name
        .clone()
        .unwrap_or_else(|| "_legacy".to_string());
    let reviewer_reset_queue = SessionScopedQueue::<ReviewerResetEntry>::new(
        &reviewer_reset_session,
        reviewer_reset_queue_dir,
    );
    for drained in reviewer_reset_queue.drain() {
        let request = match drained {
            Ok(entry) => entry.entry,
            Err(err) => {
                tracing::warn!(
                    session = reviewer_reset_session,
                    error = %err,
                    "Failed to drain reviewer reset queue entry"
                );
                continue;
            }
        };
        if ctx.mux.get(&request.reviewer).is_none() {
            push_dashboard_event(
                &ctx.dashboard_data,
                format!(
                    "dropped orphan reviewer reset for {} ({}); reviewer not in current session",
                    request.reviewer, request.task_id
                ),
            );
            continue;
        }
        let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, &request.reviewer) {
            let Some(startup_prompt) =
                build_reviewer_reset_startup_prompt(&ctx.mux, &request.reviewer)
            else {
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!(
                        "dropped reviewer reset for {} because no matching reviewer pane exists",
                        request.reviewer
                    ),
                );
                continue;
            };
            Some(startup_prompt)
        } else {
            None
        };

        let reset_reason = request.reason.clone().unwrap_or_else(|| {
            format!(
                "reviewer reset after review submission for {}",
                request.task_id
            )
        });
        let command = RuntimeCommand {
            command_id: format!("queued-reviewer-reset-{}", uuid::Uuid::new_v4()),
            target: host_runtime_target(ctx, &request.reviewer),
            issued_at_ms: runtime_command_timestamp_ms(),
            kind: RuntimeCommandKind::ResetPane {
                reason: reset_reason.clone(),
            },
        };
        let context = host_prompt_policy_context(ctx, &request.reviewer);
        if queue_runtime_command(
            ctx,
            command,
            context,
            PendingRuntimeCommandEffect::QueuedReviewerReset {
                request: request.clone(),
                startup_prompt: startup_prompt.clone(),
                brehon_root: brehon_root.to_path_buf(),
                session_name: reviewer_reset_session.clone(),
            },
        )
        .is_ok()
        {
            ctx.needs_redraw = true;
            continue;
        }

        let reset_result = if ctx.runtime_agent_factory_host_owned {
            reset_terminal_host_pane(ctx, &request.reviewer, reset_reason.clone())
        } else {
            ctx.rt
                .block_on(ctx.mux.reset_reviewer_session(&request.reviewer))
                .map_err(|err| err.to_string())
        };
        match reset_result {
            Ok(()) => {
                if let Some(startup_prompt) = startup_prompt {
                    if ctx.runtime_agent_factory_host_owned {
                        if let Err(err) = enqueue_terminal_host_startup_prompt(
                            ctx,
                            &request.reviewer,
                            startup_prompt,
                            "terminal-host reviewer reset startup prompt",
                        ) {
                            tracing::warn!(
                                reviewer = %request.reviewer,
                                task_id = %request.task_id,
                                error = %err,
                                "Failed to queue terminal-host reviewer reset startup prompt"
                            );
                        }
                    } else {
                        ctx.mux
                            .queue_startup_prompt(&request.reviewer, startup_prompt);
                    }
                }
                if let Err(err) = write_reviewer_reset_ack(brehon_root, &request) {
                    tracing::warn!(
                        reviewer = %request.reviewer,
                        task_id = %request.task_id,
                        review_id = %request.review_id,
                        error = %err,
                        "Failed to persist reviewer reset acknowledgement"
                    );
                } else {
                    push_dashboard_event(
                        &ctx.dashboard_data,
                        format!(
                            "reset reviewer {} for {} ({})",
                            request.reviewer, request.task_id, reset_reason
                        ),
                    );
                }
            }
            Err(err) => {
                let err_text = err.to_string();
                tracing::warn!(
                    reviewer = %request.reviewer,
                    task_id = %request.task_id,
                    review_id = %request.review_id,
                    error = %err_text,
                    "Failed to reset reviewer session from queue"
                );
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!(
                        "reviewer reset failed for {} on {}; will retry: {}",
                        request.reviewer, request.task_id, err_text
                    ),
                );
                if let Err(requeue_err) = reviewer_reset_queue.enqueue(request.clone()) {
                    tracing::warn!(
                        reviewer = %request.reviewer,
                        task_id = %request.task_id,
                        review_id = %request.review_id,
                        error = %requeue_err,
                        "Failed to requeue reviewer reset after reset failure"
                    );
                }
            }
        }
    }

    let worker_recycle_session = ctx
        .runtime_session_name
        .clone()
        .unwrap_or_else(|| "_legacy".to_string());
    let worker_recycle_queue = SessionScopedQueue::<WorkerRecycleEntry>::new(
        &worker_recycle_session,
        brehon_root.join("runtime").join("worker-recycle-queue"),
    );
    for scoped_request in worker_recycle_queue.drain() {
        let request = match scoped_request {
            Ok(entry) => entry.entry,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "Failed to read worker recycle queue entry; moved to dead-letter"
                );
                continue;
            }
        };
        if ctx.mux.get(&request.worker).is_none() {
            push_dashboard_event(
                &ctx.dashboard_data,
                format!(
                    "dropped orphan worker recycle for {} ({}); worker not in current session",
                    request.worker, request.task_id
                ),
            );
            continue;
        }
        let live_tasks = read_task_files(brehon_root);
        let active_task_ids: Vec<String> = live_tasks
            .iter()
            .filter(|task| {
                task.assignee.as_deref() == Some(request.worker.as_str()) && !task_is_terminal(task)
            })
            .map(|task| task.id.clone())
            .collect();
        if !active_task_ids.is_empty() {
            push_dashboard_event(
                &ctx.dashboard_data,
                format!(
                    "dropped stale worker recycle for {} after {}; worker already owns {}",
                    request.worker,
                    request.task_id,
                    active_task_ids.join(", ")
                ),
            );
            continue;
        }
        let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, &request.worker) {
            let Some(startup_prompt) =
                build_worker_recycle_startup_prompt(&ctx.mux, &request.worker)
            else {
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!(
                        "dropped worker recycle for {} because no matching worker pane exists",
                        request.worker
                    ),
                );
                continue;
            };
            Some(startup_prompt)
        } else {
            None
        };

        let reset_reason = format!(
            "worker recycle after terminal handoff for {}",
            request.task_id
        );
        let command = RuntimeCommand {
            command_id: format!("queued-worker-recycle-{}", uuid::Uuid::new_v4()),
            target: host_runtime_target(ctx, &request.worker),
            issued_at_ms: runtime_command_timestamp_ms(),
            kind: RuntimeCommandKind::ResetPane {
                reason: reset_reason.clone(),
            },
        };
        let context = host_prompt_policy_context(ctx, &request.worker);
        if queue_runtime_command(
            ctx,
            command,
            context,
            PendingRuntimeCommandEffect::QueuedWorkerRecycle {
                request: request.clone(),
                startup_prompt: startup_prompt.clone(),
                brehon_root: brehon_root.to_path_buf(),
                session_name: worker_recycle_session.clone(),
            },
        )
        .is_ok()
        {
            ctx.needs_redraw = true;
            continue;
        }

        let reset_result = if ctx.runtime_agent_factory_host_owned {
            reset_terminal_host_pane(ctx, &request.worker, reset_reason)
        } else {
            ctx.rt
                .block_on(ctx.mux.reset_worker_gateway_session(&request.worker))
                .map_err(|err| err.to_string())
        };
        match reset_result {
            Ok(()) => {
                ctx.mux.clear_pane_task_context(&request.worker);
                if let Some(startup_prompt) = startup_prompt {
                    if ctx.runtime_agent_factory_host_owned {
                        if let Err(err) = enqueue_terminal_host_startup_prompt(
                            ctx,
                            &request.worker,
                            startup_prompt,
                            "terminal-host worker recycle startup prompt",
                        ) {
                            tracing::warn!(
                                worker = %request.worker,
                                task_id = %request.task_id,
                                error = %err,
                                "Failed to queue terminal-host worker recycle startup prompt"
                            );
                        }
                    } else {
                        ctx.mux
                            .queue_startup_prompt(&request.worker, startup_prompt);
                    }
                }
                if let Err(err) = write_worker_recycle_ack(brehon_root, &request) {
                    tracing::warn!(
                        worker = %request.worker,
                        task_id = %request.task_id,
                        error = %err,
                        "Failed to persist worker recycle acknowledgement"
                    );
                } else {
                    push_dashboard_event(
                        &ctx.dashboard_data,
                        format!(
                            "recycled worker {} after terminal handoff for {}",
                            request.worker, request.task_id
                        ),
                    );
                }
            }
            Err(err) => {
                let err_text = err.to_string();
                tracing::warn!(
                    worker = %request.worker,
                    task_id = %request.task_id,
                    error = %err_text,
                    "Failed to reset worker session from recycle queue"
                );
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!(
                        "worker recycle failed for {} after {}; will retry: {}",
                        request.worker, request.task_id, err_text
                    ),
                );
                if let Err(requeue_err) = worker_recycle_queue.enqueue(request.clone()) {
                    tracing::warn!(
                        worker = %request.worker,
                        task_id = %request.task_id,
                        error = %requeue_err,
                        "Failed to requeue worker recycle request after reset failure"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_task(root: &Path, task_id: &str, status: &str, assignee: Option<&str>) {
        let tasks_dir = root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let mut task = serde_json::json!({
            "task_id": task_id,
            "title": "Example",
            "status": status,
        });
        if let Some(assignee) = assignee {
            task["assignee"] = serde_json::Value::String(assignee.to_string());
        }
        std::fs::write(
            tasks_dir.join(format!("{task_id}.json")),
            serde_json::to_string_pretty(&task).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn role_alias_prompt_target_resolves_to_active_task_assignee() {
        let temp = tempfile::tempdir().unwrap();
        write_task(temp.path(), "T-1", "in_progress", Some("safe-ewe-30"));

        let prompt = "Review complete for task T-1\n\
                      Review ID: REV-1\n\
                      Outcome: CHANGES_REQUESTED\n";

        assert_eq!(
            resolve_role_alias_prompt_target(temp.path(), "worker", prompt).unwrap(),
            Some("safe-ewe-30".to_string())
        );
    }

    #[test]
    fn role_alias_prompt_target_drops_terminal_task_reports() {
        let temp = tempfile::tempdir().unwrap();
        write_task(temp.path(), "T-1", "closed", Some("safe-ewe-30"));

        let prompt = "Review complete for task T-1\n\
                      Review ID: REV-1\n\
                      Outcome: APPROVED\n";

        assert_eq!(
            resolve_role_alias_prompt_target(temp.path(), "worker", prompt).unwrap(),
            None
        );
    }

    #[test]
    fn explicit_prompt_target_is_left_unchanged() {
        let temp = tempfile::tempdir().unwrap();

        assert_eq!(
            resolve_role_alias_prompt_target(temp.path(), "firm-hen-20", "Ping").unwrap(),
            Some("firm-hen-20".to_string())
        );
    }

    #[test]
    fn gateway_backed_targets_bypass_daemon_runtime_prompt_delivery() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        std::fs::create_dir_all(brehon_root.join("runtime").join("prompt-queue")).unwrap();
        let prompt_path = brehon_root
            .join("runtime")
            .join("prompt-queue")
            .join("queued.prompt");
        std::fs::write(&prompt_path, "queued").unwrap();

        let adapter = brehon_mux::AgentAdapter::BuiltIn(brehon_mux::SupervisorCli::Gemini);
        let pane = brehon_mux::Pane::worker(
            "worker-gemini",
            temp.path().to_path_buf(),
            Some(&brehon_root),
            "claude-supervisor",
            &adapter,
            None,
            None,
            24,
            80,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(pane.is_gateway_backed());

        let mut mux = brehon_mux::Mux::new(24, 80);
        mux.add_pane(pane);

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut ctx = crate::run::event_loop::new_headless_event_loop_ctx(
            mux,
            rt.handle().clone(),
            None,
            None,
            false,
        )
        .unwrap();
        ctx.dashboard_data.lock().unwrap().brehon_root = Some(brehon_root.clone());

        assert!(!queue_queued_prompt_delivery_via_daemon(
            &mut ctx,
            &brehon_root,
            &prompt_path,
            "worker-gemini",
            None,
            "assignment",
            Some("prompt-1"),
        ));
        assert!(ctx.pending_runtime_commands.is_empty());
    }
}

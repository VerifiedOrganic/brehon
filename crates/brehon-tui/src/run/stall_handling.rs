//! Stall detection, auto-recovery, and self-improvement logic
//! extracted from the main event loop.

use brehon_mux::{PaneKind, PaneState};
use brehon_types::config::WorkerIdleBehavior;
use brehon_types::task::TaskStatus;
use brehon_types::{RuntimeCommand, RuntimeCommandKind, RuntimePaneBlockInfo};

use super::dashboard::read_task_files;
use super::event_loop::{
    queue_runtime_command, runtime_command_target_for_pane, runtime_command_timestamp_ms,
    runtime_policy_context_for_pane, worker_recycle_command_pending,
    worker_reset_or_recycle_command_pending, EventLoopCtx, PendingRuntimeCommandEffect,
    RecoveryResetMarker,
};
use super::helpers::{
    build_supervisor_dispatch_nudge_message, compute_supervisor_dispatch_frontier,
    pane_needs_post_spawn_prompt, read_pending_review_obligations, reviewer_reset_ack_exists,
    ReviewerResetEntry,
};
use super::prompt_delivery::{
    dispatch_runtime_prompt, recycle_terminal_host_pane, reset_terminal_host_pane,
};
use super::recovery::{
    active_worker_task, agent_health_marker_reason, agent_is_quarantined_for_run,
    attempt_auto_recover_stalled_worker, block_task_for_prompt_block_recovery_failure,
    block_task_for_stalled_worker_manual_recovery, clear_agent_health_marker,
    inspect_worker_worktree_state, prompt_blocked_detail, prompt_blocked_info,
    push_dashboard_event, quarantined_supervisor_names, quarantined_worker_names,
    read_agent_health_marker, read_prompt_retry_deferral_snapshot, read_raw_task_file,
    sync_worker_task_contexts, write_prompt_blocked_recovery_failed_marker_or_clear_stale_marker,
    PROMPT_BLOCKED_HEALTH_REASON, PROMPT_BLOCKED_RECOVERY_FAILED_HEALTH_REASON,
    PROMPT_BLOCKED_RECOVERY_FAILURE_ACTIVITY, STALLED_WORKER_MANUAL_RECOVERY_ACTIVITY,
};
use super::self_improvement::{
    build_advisor_reset_startup_prompt, build_research_reset_startup_prompt,
    build_reviewer_reset_startup_prompt, build_supervisor_reset_startup_prompt,
    build_worker_context_reset_startup_prompt, build_worker_recycle_startup_prompt,
    find_review_wait_task_for_worker, next_self_improvement_prompt,
};
use super::session::read_session_files;
use super::types::{
    PendingReviewObligation, StalledRecoveryOutcome, TaskInfo, WorkerWorktreeInspection,
};

fn queue_worker_recycle(
    ctx: &mut EventLoopCtx,
    pane_id: &str,
    owning_task_id: Option<&str>,
    blocked: Option<RuntimePaneBlockInfo>,
    reason: String,
    success_message: String,
    failure_prefix: String,
    now: std::time::Instant,
) -> bool {
    if ctx.mux.get(pane_id).is_none() {
        return false;
    }
    let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, pane_id) {
        build_worker_recycle_startup_prompt(&ctx.mux, pane_id)
    } else {
        None
    };
    let command = RuntimeCommand {
        command_id: format!("worker-recycle-{}", uuid::Uuid::new_v4()),
        target: runtime_command_target_for_pane(ctx, pane_id),
        issued_at_ms: runtime_command_timestamp_ms(),
        kind: RuntimeCommandKind::RecyclePane { reason },
    };
    let context = runtime_policy_context_for_pane(ctx, pane_id);
    if queue_runtime_command(
        ctx,
        command,
        context,
        PendingRuntimeCommandEffect::WorkerRecycle {
            pane_id: pane_id.to_string(),
            owning_task_id: owning_task_id.map(str::to_string),
            blocked,
            startup_prompt,
            success_message,
            failure_prefix,
        },
    )
    .is_err()
    {
        return false;
    }

    ctx.last_activity.insert(pane_id.to_string(), now);
    ctx.pending_self_improve_prompt.remove(pane_id);
    ctx.needs_redraw = true;
    true
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ReviewRequestRecoverySnapshot {
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    context: String,
    #[serde(default)]
    commit: String,
    #[serde(default)]
    base_commit: String,
    #[serde(default)]
    merge_target_head: String,
    #[serde(default)]
    commits: Vec<String>,
    #[serde(default)]
    reviewer_prompts: std::collections::BTreeMap<String, String>,
}

fn read_review_request_recovery_snapshot(
    brehon_root: &std::path::Path,
    obligation: &PendingReviewObligation,
) -> Option<ReviewRequestRecoverySnapshot> {
    let round = obligation.round?;
    let path = brehon_root
        .join("runtime")
        .join("reviews")
        .join(&obligation.task_id)
        .join(format!("round-{round}"))
        .join("request.json");
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn push_prompt_field(out: &mut String, label: &str, value: &str) {
    let value = value.trim();
    if !value.is_empty() {
        out.push_str(label);
        out.push_str(": ");
        out.push_str(value);
        out.push('\n');
    }
}

fn build_review_obligation_recovery_prompt(
    brehon_root: &std::path::Path,
    reviewer: &str,
    obligation: &PendingReviewObligation,
    verification_cmd: &str,
) -> String {
    let mut out = format!(
        "Review-obligation nudge for reviewer '{reviewer}'.\n\
         You are still missing from active review {} for task {}: {}.\n\
         Pending reviewers remaining in this panel: {}.\n\n\
         First, call the Brehon MCP tool `{verification_cmd}` directly with \
         action=review_status task_id={} review_id={}. Do not run `{verification_cmd}` \
         through shell/Bash.\n\
         If the review is no longer collecting, the task is no longer in_review, or \
         reviewer={reviewer} is not still pending, stop and wait for the next request.\n\n",
        obligation.review_id,
        obligation.task_id,
        obligation.task_title,
        obligation.pending_reviewers,
        obligation.task_id,
        obligation.review_id,
    );

    if let Some(request) = read_review_request_recovery_snapshot(brehon_root, obligation) {
        out.push_str("Recovered review request context:\n");
        push_prompt_field(&mut out, "Task", &request.title);
        push_prompt_field(&mut out, "Description", &request.description);
        if !request.commits.is_empty() {
            out.push_str("Reviewed commits: ");
            out.push_str(&request.commits.join(", "));
            out.push('\n');
        }
        push_prompt_field(&mut out, "Commit", &request.commit);
        push_prompt_field(&mut out, "Base", &request.base_commit);
        push_prompt_field(&mut out, "Merge target head", &request.merge_target_head);
        if !request.context.trim().is_empty() {
            out.push_str("\nReview handoff context:\n");
            out.push_str(request.context.trim());
            out.push('\n');
        }

        let commit = request.commit.trim();
        if !commit.is_empty() {
            out.push_str("\nInspecting the commit:\n");
            out.push_str("- All Brehon worktrees share one .git object database. The commit is reachable from your current worktree by SHA.\n");
            out.push_str(&format!("- git show {commit} --stat\n"));
            out.push_str(&format!("- git show {commit}\n"));
            let base = request.base_commit.trim();
            let target = request.merge_target_head.trim();
            if !base.is_empty() {
                out.push_str(&format!("- git diff {base}..{commit}\n"));
            } else if !target.is_empty() {
                out.push_str(&format!("- git diff {target}..{commit}\n"));
            }
        }
    } else {
        out.push_str(
            "The persisted review request metadata was not readable. After review_status, \
             only continue if you can safely identify the reviewed diff from the active \
             task/review state; otherwise report the missing metadata to the supervisor.\n",
        );
    }

    out.push_str(&format!(
        "\nReview for correctness, security, performance, concurrency, error handling, and maintainability.\n\
         Do not edit files. Do not call request_review, reseat_panel, reassign_panel, \
         release_panel, reset_rounds, or override.\n\n\
         Submit your review with the Brehon MCP tool `{verification_cmd}` directly:\n  \
         action=submit_review review_id={} reviewer={reviewer} score=<1-10> \
         verdict=<approved|needs_revision|rejected> summary=\"Your review\" \
         findings='[{{\"description\":\"...\", \"file\":\"...\", \"line\":42, \
         \"severity\":\"blocking|suggestion|nitpick\", \"suggestion\":\"optional\"}}]'\n\
         After submitting, stop and wait for the next request.",
        obligation.review_id
    ));

    out
}

fn build_review_request_resend_prompt(
    brehon_root: &std::path::Path,
    obligation: &PendingReviewObligation,
    reviewer: &str,
) -> Option<String> {
    let request = read_review_request_recovery_snapshot(brehon_root, obligation)?;
    request.reviewer_prompts.get(reviewer).cloned()
}

type ReviewObligationTrackingKey = (String, String, String);

fn review_obligation_tracking_key(
    reviewer: &str,
    obligation: &PendingReviewObligation,
) -> ReviewObligationTrackingKey {
    (
        reviewer.to_string(),
        obligation.task_id.clone(),
        obligation.review_id.clone(),
    )
}

fn tracked_review_obligation_keys(
    obligations: &std::collections::HashMap<String, Vec<PendingReviewObligation>>,
) -> std::collections::HashSet<ReviewObligationTrackingKey> {
    let mut active_keys = std::collections::HashSet::new();
    for (reviewer, reviewer_obligations) in obligations {
        for obligation in reviewer_obligations {
            active_keys.insert(review_obligation_tracking_key(reviewer, obligation));
        }
    }
    active_keys
}

fn live_reviewer_should_receive_resend(obligation: &PendingReviewObligation) -> bool {
    obligation.assignment_acknowledged_at.is_none()
        && matches!(
            obligation.assignment_delivery_state.as_deref(),
            Some("dead_lettered" | "drained_without_ack" | "unknown")
        )
}

fn queue_reviewer_obligation_reset(
    ctx: &mut EventLoopCtx,
    brehon_root: &std::path::Path,
    reviewer: &str,
    obligation: &PendingReviewObligation,
    idle_mins: u64,
    pane_dead: bool,
    now: std::time::Instant,
) -> bool {
    let Some(pane) = ctx.mux.get(reviewer) else {
        return false;
    };
    let verification_cmd = format!("{}verification", pane.cli_type().capabilities().tool_prefix);
    let obligation_prompt = build_review_obligation_recovery_prompt(
        brehon_root,
        reviewer,
        obligation,
        &verification_cmd,
    );
    let startup_prompt = build_reviewer_reset_startup_prompt(&ctx.mux, reviewer)
        .map(|base| format!("{base}\n\n{obligation_prompt}"))
        .unwrap_or(obligation_prompt);
    let reason = if pane_dead {
        "auto-recover dead reviewer pane with pending review obligation"
    } else {
        "auto-recover idle reviewer pane with pending review obligation"
    }
    .to_string();

    let request = ReviewerResetEntry {
        task_id: obligation.task_id.clone(),
        review_id: obligation.review_id.clone(),
        reviewer: reviewer.to_string(),
        reason: Some(reason.clone()),
    };
    if reviewer_reset_ack_exists(brehon_root, &request) {
        return false;
    }
    let command = RuntimeCommand {
        command_id: format!("reviewer-obligation-reset-{}", uuid::Uuid::new_v4()),
        target: runtime_command_target_for_pane(ctx, reviewer),
        issued_at_ms: runtime_command_timestamp_ms(),
        kind: RuntimeCommandKind::ResetPane {
            reason: reason.clone(),
        },
    };
    let context = runtime_policy_context_for_pane(ctx, reviewer);
    let session_name = ctx
        .runtime_session_name
        .clone()
        .unwrap_or_else(|| "_legacy".to_string());

    if queue_runtime_command(
        ctx,
        command,
        context,
        PendingRuntimeCommandEffect::QueuedReviewerReset {
            request,
            startup_prompt: Some(startup_prompt),
            brehon_root: brehon_root.to_path_buf(),
            session_name,
        },
    )
    .is_err()
    {
        return false;
    }

    tracing::warn!(
        reviewer = %reviewer,
        task_id = %obligation.task_id,
        review_id = %obligation.review_id,
        idle_minutes = idle_mins,
        pane_dead,
        "Recovering stale reviewer obligation by resetting reviewer pane"
    );
    ctx.last_activity.insert(reviewer.to_string(), now);
    ctx.needs_redraw = true;
    true
}

fn send_review_obligation_nudge(
    ctx: &mut EventLoopCtx,
    brehon_root: &std::path::Path,
    reviewer: &str,
    obligation: &PendingReviewObligation,
    now: std::time::Instant,
) -> bool {
    let Some(pane) = ctx.mux.get(reviewer) else {
        return false;
    };
    let verification_cmd = format!("{}verification", pane.cli_type().capabilities().tool_prefix);
    let prompt = build_review_obligation_recovery_prompt(
        brehon_root,
        reviewer,
        obligation,
        &verification_cmd,
    );
    if !dispatch_runtime_prompt(ctx, reviewer, prompt, None) {
        return false;
    }
    ctx.review_obligation_notifications_sent
        .insert(review_obligation_tracking_key(reviewer, obligation), now);
    push_dashboard_event(
        &ctx.dashboard_data,
        format!(
            "nudged reviewer {} for pending review {} on {}",
            reviewer, obligation.review_id, obligation.task_id
        ),
    );
    true
}

fn resend_review_request_to_live_reviewer(
    ctx: &mut EventLoopCtx,
    brehon_root: &std::path::Path,
    reviewer: &str,
    obligation: &PendingReviewObligation,
    now: std::time::Instant,
) -> bool {
    let Some(prompt) = build_review_request_resend_prompt(brehon_root, obligation, reviewer) else {
        return false;
    };
    if !dispatch_runtime_prompt(ctx, reviewer, prompt, None) {
        return false;
    }
    ctx.review_obligation_resends_sent
        .insert(review_obligation_tracking_key(reviewer, obligation), now);
    push_dashboard_event(
        &ctx.dashboard_data,
        format!(
            "resent review request to reviewer {} for pending review {} on {} after uncertain delivery",
            reviewer, obligation.review_id, obligation.task_id
        ),
    );
    true
}

fn report_review_obligation_hard_failure(
    ctx: &mut EventLoopCtx,
    reviewer: &str,
    obligation: &PendingReviewObligation,
    detail: &str,
) {
    if !ctx
        .review_obligation_failures_reported
        .insert(review_obligation_tracking_key(reviewer, obligation))
    {
        return;
    }

    let surfaced_to_supervisor = if let Some(supervisor_id) = ctx.supervisor_id.clone() {
        dispatch_runtime_prompt(
            ctx,
            &supervisor_id,
            format!(
                "Review-obligation hard failure for reviewer '{reviewer}'.\n\
                 Task {} ({}) is still in collecting review {} on panel {} with {} pending reviewer(s), \
                 but automatic recovery could not continue because {}.\n\
                 This review must not remain silently idle. Inspect reviewer liveness, panel state, and \
                 the persisted review request before deciding whether to reseat or reset the round.",
                obligation.task_id,
                obligation.task_title,
                obligation.review_id,
                obligation.panel_id.as_deref().unwrap_or("unassigned"),
                obligation.pending_reviewers,
                detail,
            ),
            None,
        )
    } else {
        false
    };

    let event_description = if surfaced_to_supervisor {
        format!(
            "reported review-obligation hard failure for reviewer {} on {} / {}",
            reviewer, obligation.task_id, obligation.review_id
        )
    } else {
        format!(
            "review-obligation hard failure for reviewer {} on {} / {}: {}",
            reviewer, obligation.task_id, obligation.review_id, detail
        )
    };
    push_dashboard_event(&ctx.dashboard_data, event_description);
}

fn recover_stale_reviewer_obligations(
    ctx: &mut EventLoopCtx,
    brehon_root: &std::path::Path,
    obligations: &std::collections::HashMap<String, Vec<PendingReviewObligation>>,
    now: std::time::Instant,
) {
    for (reviewer, reviewer_obligations) in obligations {
        let Some(obligation) = reviewer_obligations.first() else {
            continue;
        };
        let reset_request = ReviewerResetEntry {
            task_id: obligation.task_id.clone(),
            review_id: obligation.review_id.clone(),
            reviewer: reviewer.clone(),
            reason: None,
        };
        if reviewer_reset_ack_exists(brehon_root, &reset_request) {
            continue;
        }
        let Some(pane) = ctx.mux.get(reviewer) else {
            report_review_obligation_hard_failure(
                ctx,
                reviewer,
                obligation,
                "the assigned reviewer pane is absent from the current run",
            );
            continue;
        };
        if *pane.kind() != PaneKind::Reviewer {
            report_review_obligation_hard_failure(
                ctx,
                reviewer,
                obligation,
                "the assigned pane is not a reviewer pane",
            );
            continue;
        }
        let pane_dead =
            pane.has_exited() || matches!(pane.pane_state(), Some(PaneState::Dead { .. }));
        if !pane_dead && pane.is_tool_executing() {
            continue;
        }

        let reviewer_idle = now
            .checked_duration_since(
                ctx.last_activity
                    .get(reviewer.as_str())
                    .copied()
                    .unwrap_or(std::time::Instant::now()),
            )
            .unwrap_or_default();
        if !pane_dead && reviewer_idle < ctx.review_obligation_nudge_threshold {
            continue;
        }

        let idle_mins = (reviewer_idle.as_secs() / 60).max(1);
        let nudge_key = review_obligation_tracking_key(reviewer, obligation);
        if !pane_dead {
            let nudge_sent_at = ctx
                .review_obligation_notifications_sent
                .get(&nudge_key)
                .copied();
            let resend_sent_at = ctx.review_obligation_resends_sent.get(&nudge_key).copied();
            let reset_due = nudge_sent_at.or(resend_sent_at).is_some_and(|sent_at| {
                now.duration_since(sent_at) >= ctx.review_obligation_reset_threshold
            });
            if !reset_due {
                if nudge_sent_at.is_none() && resend_sent_at.is_none() {
                    let resent = live_reviewer_should_receive_resend(obligation)
                        && resend_review_request_to_live_reviewer(
                            ctx,
                            brehon_root,
                            reviewer,
                            obligation,
                            now,
                        );
                    if resent {
                        continue;
                    }
                    if !send_review_obligation_nudge(ctx, brehon_root, reviewer, obligation, now) {
                        report_review_obligation_hard_failure(
                            ctx,
                            reviewer,
                            obligation,
                            "the recovery nudge could not be delivered",
                        );
                    }
                }
                continue;
            }
        }
        if !queue_reviewer_obligation_reset(
            ctx,
            brehon_root,
            reviewer,
            obligation,
            idle_mins,
            pane_dead,
            now,
        ) {
            report_review_obligation_hard_failure(
                ctx,
                reviewer,
                obligation,
                "the reviewer pane could not be reset",
            );
        }
    }
}

fn prune_review_obligation_tracking_records(
    ctx: &mut EventLoopCtx,
    obligations: &std::collections::HashMap<String, Vec<PendingReviewObligation>>,
) {
    let active_keys = tracked_review_obligation_keys(obligations);
    ctx.review_obligation_notifications_sent
        .retain(|key, _| active_keys.contains(key));
    ctx.review_obligation_resends_sent
        .retain(|key, _| active_keys.contains(key));
    ctx.review_obligation_failures_reported
        .retain(|key| active_keys.contains(key));
}

pub(super) fn recover_stale_deferred_prompt_delivery(
    ctx: &mut EventLoopCtx,
    target: &str,
    prompt_path: &std::path::Path,
    now: std::time::Instant,
) -> bool {
    let Some(snapshot) = read_prompt_retry_deferral_snapshot(prompt_path) else {
        return false;
    };
    let deferred_for = chrono::Utc::now()
        .signed_duration_since(snapshot.first_deferred_at)
        .to_std()
        .unwrap_or_default();
    if deferred_for < ctx.auto_recover_threshold {
        return false;
    }

    let Some(pane) = ctx.mux.get(target) else {
        return false;
    };
    if *pane.kind() != PaneKind::Worker {
        return false;
    }
    if brehon_root_for_quarantine(ctx)
        .as_ref()
        .is_some_and(|root| agent_is_quarantined_for_run(root, target))
    {
        return false;
    }

    let worker_idle = now
        .checked_duration_since(
            ctx.last_activity
                .get(target)
                .copied()
                .unwrap_or(std::time::Instant::now()),
        )
        .unwrap_or_default();
    if worker_idle < ctx.auto_recover_threshold {
        return false;
    }

    let idle_mins = (worker_idle.as_secs() / 60).max(1);
    let deferred_mins = (deferred_for.as_secs() / 60).max(1);
    let reason = snapshot
        .reason
        .unwrap_or_else(|| "queued prompt delivery deferred".to_string());
    tracing::warn!(
        worker = %target,
        prompt_path = %prompt_path.display(),
        deferrals = snapshot.deferrals,
        deferred_for_ms = %deferred_for.as_millis(),
        idle_ms = %worker_idle.as_millis(),
        last_deferred_at = %snapshot.last_deferred_at.to_rfc3339(),
        reason = %reason,
        "Recovering worker after stale queued prompt delivery deferrals"
    );

    queue_worker_recycle(
        ctx,
        target,
        None,
        None,
        "auto-recover worker after stale queued prompt delivery via daemon recycle".to_string(),
        format!(
            "recycled worker {target} after queued prompt delivery stalled {deferred_mins}m and pane was idle {idle_mins}m via daemon recycle"
        ),
        format!("failed to recycle worker {target} after stale queued prompt delivery"),
        now,
    )
}

fn brehon_root_for_quarantine(ctx: &EventLoopCtx) -> Option<std::path::PathBuf> {
    ctx.dashboard_data.lock().ok()?.brehon_root.clone()
}

fn queue_worker_context_reset(
    ctx: &mut EventLoopCtx,
    pane_id: &str,
    reason: String,
    success_message: String,
    failure_prefix: String,
    now: std::time::Instant,
) -> bool {
    if ctx.mux.get(pane_id).is_none() {
        return false;
    }
    let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, pane_id) {
        build_worker_context_reset_startup_prompt(&ctx.mux, pane_id)
    } else {
        None
    };
    let command = RuntimeCommand {
        command_id: format!("worker-context-reset-{}", uuid::Uuid::new_v4()),
        target: runtime_command_target_for_pane(ctx, pane_id),
        issued_at_ms: runtime_command_timestamp_ms(),
        kind: RuntimeCommandKind::ResetPane { reason },
    };
    let context = runtime_policy_context_for_pane(ctx, pane_id);
    if queue_runtime_command(
        ctx,
        command,
        context,
        PendingRuntimeCommandEffect::RecoveryReset {
            pane_id: pane_id.to_string(),
            startup_prompt,
            success_message,
            failure_prefix,
            marker: RecoveryResetMarker::WorkerContext,
        },
    )
    .is_err()
    {
        return false;
    }

    ctx.last_activity.insert(pane_id.to_string(), now);
    ctx.pending_self_improve_prompt.remove(pane_id);
    ctx.needs_redraw = true;
    true
}

fn prompt_blocked_recoverable_pane_kind(kind: &PaneKind) -> bool {
    matches!(
        kind,
        PaneKind::Worker
            | PaneKind::Reviewer
            | PaneKind::Advisor
            | PaneKind::Research
            | PaneKind::Supervisor
    )
}

fn prompt_blocked_recoverable_session_role(role: &str) -> bool {
    matches!(
        role,
        "worker" | "reviewer" | "advisor" | "research" | "supervisor"
    )
}

fn prompt_blocked_session_pane_kind(
    sessions: &std::collections::HashMap<String, (String, String, String)>,
    pane_id: &str,
) -> Option<PaneKind> {
    let (role, _, _) = sessions.get(pane_id)?;
    let role = role.as_str();
    match role {
        "worker" => Some(PaneKind::Worker),
        "reviewer" => Some(PaneKind::Reviewer),
        "advisor" => Some(PaneKind::Advisor),
        "research" => Some(PaneKind::Research),
        "supervisor" => Some(PaneKind::Supervisor),
        _ => None,
    }
}

fn prompt_blocked_agent_names(
    ctx: &mut EventLoopCtx,
    brehon_root: &std::path::Path,
    tasks: &[TaskInfo],
    sessions: &std::collections::HashMap<String, (String, String, String)>,
) -> Vec<String> {
    let mut agents = std::collections::BTreeSet::new();
    let mut cleared_suppressions = Vec::new();
    for pane in ctx.mux.panes() {
        if !prompt_blocked_recoverable_pane_kind(pane.kind()) {
            continue;
        }
        let pane_id = pane.id();
        let marker = read_agent_health_marker(brehon_root, pane_id);
        let marker_prompt_blocked = marker.as_ref().and_then(|marker| marker.reason.as_deref())
            == Some(PROMPT_BLOCKED_HEALTH_REASON);
        let pane_prompt_blocked = matches!(pane.pane_state(), Some(PaneState::Blocked { .. }));
        if ctx.prompt_blocked_recovery_failed_panes.contains(pane_id) {
            if !(marker_prompt_blocked || pane_prompt_blocked) {
                cleared_suppressions.push(pane_id.to_string());
            }
            continue;
        }
        if marker.as_ref().and_then(|marker| marker.reason.as_deref())
            == Some(PROMPT_BLOCKED_RECOVERY_FAILED_HEALTH_REASON)
        {
            continue;
        }
        if prompt_blocked_recovery_failure_already_recorded(
            tasks,
            prompt_blocked_owned_task_id(ctx, brehon_root, pane_id).as_deref(),
        ) {
            continue;
        }
        if marker_prompt_blocked || pane_prompt_blocked {
            agents.insert(pane_id.to_string());
        }
    }
    for (name, (role, _, _)) in sessions {
        let marker = read_agent_health_marker(brehon_root, name);
        let marker_prompt_blocked = marker.as_ref().and_then(|marker| marker.reason.as_deref())
            == Some(PROMPT_BLOCKED_HEALTH_REASON);
        if ctx.prompt_blocked_recovery_failed_panes.contains(name) {
            if !marker_prompt_blocked {
                cleared_suppressions.push(name.clone());
            }
            continue;
        }
        if marker.as_ref().and_then(|marker| marker.reason.as_deref())
            == Some(PROMPT_BLOCKED_RECOVERY_FAILED_HEALTH_REASON)
        {
            continue;
        }
        if prompt_blocked_recoverable_session_role(role)
            && marker_prompt_blocked
            && !prompt_blocked_recovery_failure_already_recorded(
                tasks,
                prompt_blocked_owned_task_id(ctx, brehon_root, name).as_deref(),
            )
        {
            agents.insert(name.clone());
        }
    }
    for pane_id in cleared_suppressions {
        ctx.prompt_blocked_recovery_failed_panes.remove(&pane_id);
    }
    agents.into_iter().collect()
}

fn prompt_blocked_marker_detail(brehon_root: &std::path::Path, worker_id: &str) -> Option<String> {
    let marker = read_agent_health_marker(brehon_root, worker_id)?;
    if marker.reason.as_deref() != Some(PROMPT_BLOCKED_HEALTH_REASON) {
        return None;
    }
    Some(prompt_blocked_detail(&marker.blocked?))
}

fn prompt_blocked_pane_detail(ctx: &EventLoopCtx, worker_id: &str) -> Option<String> {
    let pane = ctx.mux.get(worker_id)?;
    let PaneState::Blocked { info, .. } = pane.pane_state()? else {
        return None;
    };
    Some(prompt_blocked_detail(info))
}

fn pane_task_context_is_active_for_prompt_blocked(pane: &brehon_mux::Pane) -> Option<String> {
    let task = pane.task_context()?;
    // TaskContextSnapshot normalizes both raw `review_ready` and `in_review`
    // task files to `TaskStatus::InReview`, so this fallback intentionally
    // treats `InReview` as covering both worker-reserving states.
    matches!(
        task.status,
        TaskStatus::Assigned
            | TaskStatus::InProgress
            | TaskStatus::InReview
            | TaskStatus::ChangesRequested
            | TaskStatus::Approved
    )
    .then(|| task.task_id.clone())
}

fn prompt_blocked_active_task_id(
    ctx: &EventLoopCtx,
    tasks: &[TaskInfo],
    worker_id: &str,
) -> Option<String> {
    active_assigned_task_for_worker(tasks, worker_id)
        .map(|task| task.id.clone())
        .or_else(|| {
            ctx.mux
                .get(worker_id)
                .and_then(pane_task_context_is_active_for_prompt_blocked)
        })
}

fn prompt_blocked_owned_task_id(
    ctx: &EventLoopCtx,
    brehon_root: &std::path::Path,
    worker_id: &str,
) -> Option<String> {
    let pane = ctx.mux.get(worker_id);
    read_agent_health_marker(brehon_root, worker_id)
        .filter(|marker| marker.reason.as_deref() == Some(PROMPT_BLOCKED_HEALTH_REASON))
        .and_then(|marker| marker.blocked.and_then(|blocked| blocked.task_id))
        .or_else(|| {
            pane.as_ref().and_then(|pane| match pane.pane_state() {
                Some(PaneState::Blocked { info, .. }) => info.task_id.clone(),
                _ => None,
            })
        })
        .or_else(|| {
            pane.as_ref().and_then(|pane| {
                pane.review_context()
                    .map(|review| review.task_id.clone())
                    .or_else(|| pane.task_context().map(|task| task.task_id.clone()))
            })
        })
}

fn prompt_blocked_recovery_failure_already_recorded(
    tasks: &[TaskInfo],
    task_id: Option<&str>,
) -> bool {
    let Some(task_id) = task_id else {
        return false;
    };
    tasks.iter().any(|task| {
        task.id == task_id
            && brehon_types::task::normalize_task_status(&task.status) == Some("blocked")
            && task.activity.as_deref() == Some(PROMPT_BLOCKED_RECOVERY_FAILURE_ACTIVITY)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptBlockedRecoveryOutcome {
    RecoveredNoTaskChange,
    TaskModified,
}

impl PromptBlockedRecoveryOutcome {
    fn task_files_modified(self) -> bool {
        matches!(self, Self::TaskModified)
    }
}

fn terminally_mark_taskless_prompt_block_recovery_failure(
    ctx: &mut EventLoopCtx,
    brehon_root: &std::path::Path,
    pane_id: &str,
    failure: &str,
) -> PromptBlockedRecoveryOutcome {
    let blocked = prompt_blocked_info(brehon_root, pane_id, ctx.mux.get(pane_id));
    match write_prompt_blocked_recovery_failed_marker_or_clear_stale_marker(
        brehon_root,
        pane_id,
        failure,
        blocked.as_ref(),
    ) {
        Ok(()) => {
            push_dashboard_event(&ctx.dashboard_data, failure.to_string());
            PromptBlockedRecoveryOutcome::RecoveredNoTaskChange
        }
        Err(marker_err) => {
            tracing::warn!(
                pane = %pane_id,
                error = %marker_err,
                "Failed to persist prompt-blocked terminal recovery failure marker"
            );
            ctx.prompt_blocked_recovery_failed_panes
                .insert(pane_id.to_string());
            push_dashboard_event(&ctx.dashboard_data, failure.to_string());
            PromptBlockedRecoveryOutcome::RecoveredNoTaskChange
        }
    }
}

fn persist_task_backed_prompt_blocked_recovery_failure(
    ctx: &mut EventLoopCtx,
    brehon_root: &std::path::Path,
    pane_id: &str,
    task_id: &str,
    failure: &str,
    task_err: &str,
    blocked: Option<RuntimePaneBlockInfo>,
) -> PromptBlockedRecoveryOutcome {
    let operator_failure = format!("{failure}; could not mark task {task_id} blocked: {task_err}");
    let blocked =
        blocked.or_else(|| prompt_blocked_info(brehon_root, pane_id, ctx.mux.get(pane_id)));
    match write_prompt_blocked_recovery_failed_marker_or_clear_stale_marker(
        brehon_root,
        pane_id,
        &operator_failure,
        blocked.as_ref(),
    ) {
        Ok(()) => {
            push_dashboard_event(&ctx.dashboard_data, operator_failure);
            PromptBlockedRecoveryOutcome::RecoveredNoTaskChange
        }
        Err(marker_err) => {
            tracing::warn!(
                pane = %pane_id,
                task_id,
                error = %marker_err,
                "Failed to persist prompt-blocked terminal recovery failure marker after task update failed"
            );
            ctx.prompt_blocked_recovery_failed_panes
                .insert(pane_id.to_string());
            push_dashboard_event(&ctx.dashboard_data, operator_failure);
            PromptBlockedRecoveryOutcome::RecoveredNoTaskChange
        }
    }
}

fn prompt_blocked_reset_startup_prompt(
    ctx: &EventLoopCtx,
    pane_id: &str,
    kind: &PaneKind,
) -> Option<String> {
    if !pane_needs_post_spawn_prompt(&ctx.mux, pane_id) {
        return None;
    }
    match kind {
        PaneKind::Worker => build_worker_context_reset_startup_prompt(&ctx.mux, pane_id),
        PaneKind::Reviewer => build_reviewer_reset_startup_prompt(&ctx.mux, pane_id),
        PaneKind::Advisor => build_advisor_reset_startup_prompt(&ctx.mux, pane_id),
        PaneKind::Research => build_research_reset_startup_prompt(&ctx.mux, pane_id),
        PaneKind::Supervisor => build_supervisor_reset_startup_prompt(
            &ctx.mux,
            pane_id,
            ctx.runtime_agent_factory_host_owned,
        ),
        PaneKind::Director | PaneKind::Shell => None,
    }
}

fn enqueue_prompt_blocked_reset_startup_prompt(
    ctx: &mut EventLoopCtx,
    pane_id: &str,
    pane_kind: &PaneKind,
    startup_prompt: String,
) {
    if ctx.runtime_agent_factory_host_owned {
        let reason = format!(
            "terminal-host prompt-blocked {} reset startup prompt",
            pane_kind.as_str()
        );
        if let Err(err) = super::prompt_delivery::enqueue_terminal_host_startup_prompt(
            ctx,
            pane_id,
            startup_prompt,
            &reason,
        ) {
            tracing::warn!(
                pane = %pane_id,
                kind = pane_kind.as_str(),
                error = %err,
                "Failed to queue prompt-blocked reset startup prompt"
            );
        }
    } else {
        ctx.mux.queue_startup_prompt(pane_id, startup_prompt);
    }
}

fn recover_prompt_blocked_missing_pane(
    ctx: &mut EventLoopCtx,
    brehon_root: &std::path::Path,
    pane_id: &str,
    agent_label: &str,
    owning_task_id: Option<&str>,
    detail: &str,
) -> PromptBlockedRecoveryOutcome {
    let failure =
        format!("prompt-blocked {agent_label} {pane_id} no longer has a live pane: {detail}");
    tracing::warn!(pane = %pane_id, kind = agent_label, detail, "{failure}");

    if let Some(task_id) = owning_task_id {
        let blocked_task_result =
            block_task_for_prompt_block_recovery_failure(brehon_root, task_id, pane_id, &failure);
        match blocked_task_result {
            Ok(()) => {
                clear_agent_health_marker(brehon_root, pane_id);
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!(
                        "blocked task {task_id} after prompt-blocked {agent_label} {pane_id} lost its live pane"
                    ),
                );
                PromptBlockedRecoveryOutcome::TaskModified
            }
            Err(task_err) => {
                tracing::warn!(
                    pane = %pane_id,
                    task_id,
                    error = %task_err,
                    "Failed to mark prompt-blocked task as blocked after pane disappeared"
                );
                persist_task_backed_prompt_blocked_recovery_failure(
                    ctx,
                    brehon_root,
                    pane_id,
                    task_id,
                    &failure,
                    &task_err,
                    None,
                )
            }
        }
    } else {
        clear_agent_health_marker(brehon_root, pane_id);
        push_dashboard_event(
            &ctx.dashboard_data,
            format!(
                "cleared stale prompt-blocked marker for {agent_label} {pane_id} after pane disappeared"
            ),
        );
        PromptBlockedRecoveryOutcome::RecoveredNoTaskChange
    }
}

fn reset_prompt_blocked_non_worker(
    ctx: &mut EventLoopCtx,
    brehon_root: &std::path::Path,
    pane_id: &str,
    pane_kind: PaneKind,
    owning_task_id: Option<&str>,
    detail: &str,
    now: std::time::Instant,
) -> PromptBlockedRecoveryOutcome {
    let startup_prompt = prompt_blocked_reset_startup_prompt(ctx, pane_id, &pane_kind);
    let reset_reason = format!(
        "auto-recover prompt-blocked {} pane: {detail}",
        pane_kind.as_str()
    );
    let reset_result = if ctx.runtime_agent_factory_host_owned {
        super::prompt_delivery::reset_terminal_host_pane(ctx, pane_id, reset_reason.clone())
    } else {
        match pane_kind {
            PaneKind::Reviewer => ctx
                .rt
                .block_on(ctx.mux.reset_reviewer_session(pane_id))
                .map_err(|err| err.to_string()),
            PaneKind::Advisor => ctx
                .rt
                .block_on(ctx.mux.reset_advisor_session(pane_id))
                .map_err(|err| err.to_string()),
            PaneKind::Research => ctx
                .rt
                .block_on(ctx.mux.reset_research_session(pane_id))
                .map_err(|err| err.to_string()),
            PaneKind::Supervisor => ctx
                .rt
                .block_on(ctx.mux.reset_supervisor_session(pane_id))
                .map_err(|err| err.to_string()),
            PaneKind::Worker | PaneKind::Director | PaneKind::Shell => Err(format!(
                "unsupported prompt-blocked auto-reset for {} pane {pane_id}",
                pane_kind.as_str()
            )),
        }
    };

    match reset_result {
        Ok(()) => {
            clear_agent_health_marker(brehon_root, pane_id);
            if let Some(startup_prompt) = startup_prompt {
                enqueue_prompt_blocked_reset_startup_prompt(
                    ctx,
                    pane_id,
                    &pane_kind,
                    startup_prompt,
                );
            }
            ctx.last_activity.insert(pane_id.to_string(), now);
            ctx.pending_self_improve_prompt.remove(pane_id);
            if matches!(pane_kind, PaneKind::Supervisor) {
                ctx.last_supervisor_reset.insert(pane_id.to_string(), now);
            }
            let event = if let Some(task_id) = owning_task_id {
                format!(
                    "reset prompt-blocked {} {} for {task_id} after {detail}",
                    pane_kind.as_str(),
                    pane_id,
                )
            } else {
                format!(
                    "reset prompt-blocked {} {} after {detail}",
                    pane_kind.as_str(),
                    pane_id,
                )
            };
            push_dashboard_event(&ctx.dashboard_data, event.clone());
            tracing::warn!(pane = %pane_id, kind = pane_kind.as_str(), "{event}");
            PromptBlockedRecoveryOutcome::RecoveredNoTaskChange
        }
        Err(err) => {
            let failure = format!(
                "failed to reset prompt-blocked {} {pane_id}: {detail}; reset error: {err}",
                pane_kind.as_str(),
            );
            tracing::warn!(
                pane = %pane_id,
                kind = pane_kind.as_str(),
                error = %err,
                detail,
                "{failure}"
            );
            if let Some(task_id) = owning_task_id {
                let blocked_task_result = block_task_for_prompt_block_recovery_failure(
                    brehon_root,
                    task_id,
                    pane_id,
                    &failure,
                );
                match blocked_task_result {
                    Ok(()) => {
                        clear_agent_health_marker(brehon_root, pane_id);
                        push_dashboard_event(
                            &ctx.dashboard_data,
                            format!(
                                "blocked task {task_id} after prompt-blocked {} {pane_id} could not be reset",
                                pane_kind.as_str(),
                            ),
                        );
                        PromptBlockedRecoveryOutcome::TaskModified
                    }
                    Err(task_err) => {
                        tracing::warn!(
                            pane = %pane_id,
                            task_id,
                            error = %task_err,
                            "Failed to mark prompt-blocked task as blocked after reset failure"
                        );
                        persist_task_backed_prompt_blocked_recovery_failure(
                            ctx,
                            brehon_root,
                            pane_id,
                            task_id,
                            &failure,
                            &task_err,
                            None,
                        )
                    }
                }
            } else {
                terminally_mark_taskless_prompt_block_recovery_failure(
                    ctx,
                    brehon_root,
                    pane_id,
                    &failure,
                )
            }
        }
    }
}

fn recover_prompt_blocked_worker(
    ctx: &mut EventLoopCtx,
    brehon_root: &std::path::Path,
    worker_id: &str,
    active_task_id: Option<&str>,
    owning_task_id: Option<&str>,
    detail: &str,
    now: std::time::Instant,
) -> PromptBlockedRecoveryOutcome {
    if active_task_id.is_none() {
        if worker_recycle_command_pending(ctx, worker_id) {
            return PromptBlockedRecoveryOutcome::RecoveredNoTaskChange;
        }
        let blocked = prompt_blocked_info(brehon_root, worker_id, ctx.mux.get(worker_id));
        let recycle_reason = format!("auto-recover prompt-blocked idle worker pane: {detail}");
        let success_message =
            format!("recycled prompt-blocked idle worker {worker_id} after {detail}");
        if queue_worker_recycle(
            ctx,
            worker_id,
            owning_task_id,
            blocked,
            recycle_reason.clone(),
            success_message.clone(),
            format!("failed to recycle prompt-blocked idle worker {worker_id}"),
            now,
        ) {
            return PromptBlockedRecoveryOutcome::RecoveredNoTaskChange;
        }

        let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, worker_id) {
            build_worker_recycle_startup_prompt(&ctx.mux, worker_id)
        } else {
            None
        };
        let recycle_result = if ctx.runtime_agent_factory_host_owned {
            recycle_terminal_host_pane(ctx, worker_id, recycle_reason.clone())
        } else {
            let generation = ctx.rt.block_on(ctx.mux.recycle(worker_id, &recycle_reason));
            (generation.0 > 0)
                .then_some(())
                .ok_or_else(|| "authoritative recycle failed".to_string())
        };

        return match recycle_result {
            Ok(()) => {
                clear_agent_health_marker(brehon_root, worker_id);
                ctx.mux.clear_pane_task_context(worker_id);
                if let Some(startup_prompt) = startup_prompt {
                    if ctx.runtime_agent_factory_host_owned {
                        if let Err(err) =
                            super::prompt_delivery::enqueue_terminal_host_startup_prompt(
                                ctx,
                                worker_id,
                                startup_prompt,
                                "terminal-host prompt-blocked idle worker recycle startup prompt",
                            )
                        {
                            tracing::warn!(
                                worker = %worker_id,
                                error = %err,
                                "Failed to queue prompt-blocked idle worker recycle startup prompt"
                            );
                        }
                    } else {
                        ctx.mux.queue_startup_prompt(worker_id, startup_prompt);
                    }
                }
                ctx.last_activity.insert(worker_id.to_string(), now);
                ctx.pending_self_improve_prompt.remove(worker_id);
                push_dashboard_event(&ctx.dashboard_data, success_message.clone());
                tracing::warn!(worker = %worker_id, "{success_message}");
                PromptBlockedRecoveryOutcome::RecoveredNoTaskChange
            }
            Err(err) => {
                let failure = format!(
                    "failed to recycle prompt-blocked idle worker {worker_id}: {detail}; recycle error: {err}"
                );
                tracing::warn!(worker = %worker_id, error = %err, detail, "{failure}");
                if let Some(task_id) = owning_task_id {
                    let blocked_task_result = block_task_for_prompt_block_recovery_failure(
                        brehon_root,
                        task_id,
                        worker_id,
                        &failure,
                    );
                    match blocked_task_result {
                        Ok(()) => {
                            clear_agent_health_marker(brehon_root, worker_id);
                            push_dashboard_event(
                                &ctx.dashboard_data,
                                format!(
                                    "blocked task {task_id} after prompt-blocked idle worker {worker_id} could not be recycled"
                                ),
                            );
                            PromptBlockedRecoveryOutcome::TaskModified
                        }
                        Err(task_err) => {
                            tracing::warn!(
                                worker = %worker_id,
                                task_id,
                                error = %task_err,
                                "Failed to mark prompt-blocked task as blocked after recycle failure"
                            );
                            persist_task_backed_prompt_blocked_recovery_failure(
                                ctx,
                                brehon_root,
                                worker_id,
                                task_id,
                                &failure,
                                &task_err,
                                None,
                            )
                        }
                    }
                } else {
                    terminally_mark_taskless_prompt_block_recovery_failure(
                        ctx,
                        brehon_root,
                        worker_id,
                        &failure,
                    )
                }
            }
        };
    }

    let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, worker_id) {
        build_worker_context_reset_startup_prompt(&ctx.mux, worker_id)
    } else {
        None
    };
    let reset_reason = format!("auto-recover prompt-blocked worker pane: {detail}");
    let reset_result = if ctx.runtime_agent_factory_host_owned {
        super::prompt_delivery::reset_terminal_host_pane(ctx, worker_id, reset_reason.clone())
    } else {
        ctx.rt
            .block_on(ctx.mux.reset_worker_gateway_session(worker_id))
            .map_err(|err| err.to_string())
    };

    match reset_result {
        Ok(()) => {
            clear_agent_health_marker(brehon_root, worker_id);
            if let Some(startup_prompt) = startup_prompt {
                if ctx.runtime_agent_factory_host_owned {
                    if let Err(err) = super::prompt_delivery::enqueue_terminal_host_startup_prompt(
                        ctx,
                        worker_id,
                        startup_prompt,
                        "terminal-host prompt-blocked worker reset startup prompt",
                    ) {
                        tracing::warn!(
                            worker = %worker_id,
                            error = %err,
                            "Failed to queue prompt-blocked worker startup prompt"
                        );
                    }
                } else {
                    ctx.mux.queue_startup_prompt(worker_id, startup_prompt);
                }
            }
            ctx.last_activity.insert(worker_id.to_string(), now);
            ctx.last_worker_context_reset
                .insert(worker_id.to_string(), now);
            ctx.pending_self_improve_prompt.remove(worker_id);
            let task_id = active_task_id.expect("active task checked above");
            let event =
                format!("reset prompt-blocked worker {worker_id} for {task_id} after {detail}");
            push_dashboard_event(&ctx.dashboard_data, event.clone());
            tracing::warn!(worker = %worker_id, active_task_id, "{event}");
            PromptBlockedRecoveryOutcome::RecoveredNoTaskChange
        }
        Err(err) => {
            let failure = format!(
                "failed to reset prompt-blocked worker {worker_id}: {detail}; reset error: {err}"
            );
            tracing::warn!(worker = %worker_id, error = %err, detail, "{failure}");
            if let Some(task_id) = active_task_id.or(owning_task_id) {
                let blocked_task_result = block_task_for_prompt_block_recovery_failure(
                    brehon_root,
                    task_id,
                    worker_id,
                    &failure,
                );
                return match blocked_task_result {
                    Ok(()) => {
                        clear_agent_health_marker(brehon_root, worker_id);
                        push_dashboard_event(
                            &ctx.dashboard_data,
                            format!(
                                "blocked task {task_id} after prompt-blocked worker {worker_id} could not be reset"
                            ),
                        );
                        PromptBlockedRecoveryOutcome::TaskModified
                    }
                    Err(task_err) => {
                        tracing::warn!(
                            worker = %worker_id,
                            task_id,
                            error = %task_err,
                            "Failed to mark prompt-blocked task as blocked after reset failure"
                        );
                        persist_task_backed_prompt_blocked_recovery_failure(
                            ctx,
                            brehon_root,
                            worker_id,
                            task_id,
                            &failure,
                            &task_err,
                            None,
                        )
                    }
                };
            } else {
                return terminally_mark_taskless_prompt_block_recovery_failure(
                    ctx,
                    brehon_root,
                    worker_id,
                    &failure,
                );
            }
        }
    }
}

fn queue_supervisor_context_reset(
    ctx: &mut EventLoopCtx,
    pane_id: &str,
    reason: String,
    success_message: String,
    failure_prefix: String,
    now: std::time::Instant,
) -> bool {
    let Some(pane) = ctx.mux.get(pane_id) else {
        return false;
    };
    if pane.kind() != &PaneKind::Supervisor {
        return false;
    }
    let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, pane_id) {
        build_supervisor_reset_startup_prompt(
            &ctx.mux,
            pane_id,
            ctx.runtime_agent_factory_host_owned,
        )
    } else {
        None
    };
    let command = RuntimeCommand {
        command_id: format!("supervisor-context-reset-{}", uuid::Uuid::new_v4()),
        target: runtime_command_target_for_pane(ctx, pane_id),
        issued_at_ms: runtime_command_timestamp_ms(),
        kind: RuntimeCommandKind::ResetPane { reason },
    };
    let context = runtime_policy_context_for_pane(ctx, pane_id);
    if queue_runtime_command(
        ctx,
        command,
        context,
        PendingRuntimeCommandEffect::RecoveryReset {
            pane_id: pane_id.to_string(),
            startup_prompt,
            success_message,
            failure_prefix,
            marker: RecoveryResetMarker::Supervisor,
        },
    )
    .is_err()
    {
        return false;
    }

    ctx.last_activity.insert(pane_id.to_string(), now);
    ctx.last_supervisor_reset.insert(pane_id.to_string(), now);
    ctx.pending_self_improve_prompt.remove(pane_id);
    ctx.needs_redraw = true;
    true
}

fn active_assigned_task_for_worker<'a>(
    tasks: &'a [TaskInfo],
    worker_id: &str,
) -> Option<&'a TaskInfo> {
    tasks
        .iter()
        .find(|task| task.task_type == "task" && active_worker_task(task, worker_id))
}

fn active_worker_recovery_key(worker_id: &str, task_id: &str) -> (String, String) {
    (worker_id.to_string(), task_id.to_string())
}

fn worker_sessions_available_for_auto_recovery(
    ctx: &EventLoopCtx,
    brehon_root: &std::path::Path,
    sessions: &std::collections::HashMap<String, (String, String, String)>,
) -> std::collections::HashMap<String, (String, String, String)> {
    sessions
        .iter()
        .filter_map(|(worker_name, (role, session_id, last_seen_at))| {
            if role != "worker" {
                return None;
            }
            if agent_is_quarantined_for_run(brehon_root, worker_name)
                || worker_reset_or_recycle_command_pending(ctx, worker_name)
            {
                return None;
            }
            let pane = ctx.mux.get(worker_name)?;
            if *pane.kind() != PaneKind::Worker
                || pane.has_exited()
                || pane.is_tool_executing()
                || matches!(
                    pane.pane_state(),
                    Some(
                        PaneState::Busy { .. } | PaneState::Blocked { .. } | PaneState::Dead { .. }
                    )
                )
            {
                return None;
            }
            Some((
                worker_name.clone(),
                (role.clone(), session_id.clone(), last_seen_at.clone()),
            ))
        })
        .collect()
}

fn current_worker_idle_duration(
    ctx: &EventLoopCtx,
    worker_id: &str,
    now: std::time::Instant,
) -> std::time::Duration {
    now.checked_duration_since(
        ctx.last_activity
            .get(worker_id)
            .copied()
            .unwrap_or(std::time::Instant::now()),
    )
    .unwrap_or_default()
}

fn build_recovered_task_assignment_prompt(
    task: &TaskInfo,
    old_worker: &str,
    idle_mins: u64,
    task_cmd: &str,
) -> String {
    format!(
        "You have been assigned recovered task {}: {}\n\
         Call the task tool with `{task_cmd} action=mine` to see your assigned tasks, \
         then begin working on them.\n\n\
         Brehon automatically recovered this task after worker '{old_worker}' was idle for \
         {idle_mins}m without pane output and their worktree inspected clean.\n\
         Resume from the current worktree state; do not restart from scratch, change \
         branches, or discard local work.",
        task.id, task.title
    )
}

fn send_active_worker_recovery_nudge(
    ctx: &mut EventLoopCtx,
    worker_id: &str,
    task: &TaskInfo,
    idle_mins: u64,
    now: std::time::Instant,
) -> bool {
    let Some(pane) = ctx.mux.get(worker_id) else {
        return false;
    };
    let task_cmd = format!("{}task", pane.cli_type().capabilities().tool_prefix);
    let prompt = format!(
        "Worker liveness nudge for '{worker_id}'. Brehon still shows you assigned task {}: {} (status `{}`). This pane has been silent for {idle_mins}m.\n\
         If you are actively working, reply now with a concise status or report progress through `{task_cmd}` as normal.\n\
         If you lost task context, call `{task_cmd} action=mine` at most once, then resume from the current worktree. Do not restart from scratch, change branches, or discard local work.",
        task.id, task.title, task.status,
    );
    if !dispatch_runtime_prompt(ctx, worker_id, prompt, None) {
        return false;
    }
    ctx.active_worker_recovery_nudges_sent
        .insert(active_worker_recovery_key(worker_id, &task.id), now);
    push_dashboard_event(
        &ctx.dashboard_data,
        format!(
            "nudged assigned worker {} for stale task {} after {}m idle",
            worker_id, task.id, idle_mins
        ),
    );
    true
}

fn send_no_task_progress_recovery_nudge(
    ctx: &mut EventLoopCtx,
    worker_id: &str,
    task: &TaskInfo,
    assigned_mins: u64,
    now: std::time::Instant,
) -> bool {
    let Some(pane) = ctx.mux.get(worker_id) else {
        return false;
    };
    let task_cmd = format!("{}task", pane.cli_type().capabilities().tool_prefix);
    let prompt = format!(
        "[brehon] No task progress recorded for {task_id}: {title}\n\n\
         This task was assigned to you {assigned_mins}m ago, but Brehon has not seen a \
         progress, checkpoint, complete, or blocked task call for this assignment. \
         Terminal output, prose, and exploratory tool calls do not update task lifecycle state.\n\n\
         Make exactly one Brehon task call now:\n\
         - Still working: `{task_cmd} action=progress id={task_id} percent=<n> notes=\"<status>\" activity=<reading|editing|testing|reviewing>`\n\
         - Ready for review: `{task_cmd} action=complete id={task_id} notes=\"<summary>\" activity=testing`\n\
         - Blocked: `{task_cmd} action=update id={task_id} status=blocked blockers=\"<reason>\"`\n\n\
         Do not narrate a plan. Call the Brehon MCP tool.",
        task_id = task.id,
        title = task.title,
    );
    if !dispatch_runtime_prompt(ctx, worker_id, prompt, None) {
        return false;
    }
    ctx.active_worker_recovery_nudges_sent
        .insert(active_worker_recovery_key(worker_id, &task.id), now);
    push_dashboard_event(
        &ctx.dashboard_data,
        format!(
            "nudged assigned worker {} for task {} after {}m without Brehon task progress",
            worker_id, task.id, assigned_mins
        ),
    );
    true
}

fn send_dirty_worktree_handoff_nudge(
    ctx: &mut EventLoopCtx,
    worker_id: &str,
    task: &TaskInfo,
    reason: &str,
    idle_secs: u64,
    now: std::time::Instant,
) -> bool {
    let Some(pane) = ctx.mux.get(worker_id) else {
        return false;
    };
    let task_cmd = format!("{}task", pane.cli_type().capabilities().tool_prefix);
    let prompt = build_dirty_worktree_handoff_nudge_message(task, &task_cmd, reason, idle_secs);
    if !dispatch_runtime_prompt(ctx, worker_id, prompt, None) {
        return false;
    }
    ctx.active_worker_recovery_nudges_sent
        .insert(active_worker_recovery_key(worker_id, &task.id), now);
    push_dashboard_event(
        &ctx.dashboard_data,
        format!(
            "nudged {worker_id} to checkpoint, complete, or block {} after dirty worktree handoff gap ({idle_secs}s idle)",
            task.id
        ),
    );
    true
}

fn task_assignment_without_progress_age(
    brehon_root: &std::path::Path,
    task: &TaskInfo,
    worker_id: &str,
    now_utc: chrono::DateTime<chrono::Utc>,
) -> Option<std::time::Duration> {
    let raw = read_raw_task_file(brehon_root, &task.id)?;
    let propagation = raw.get("assignment_propagation")?.as_object()?;
    if propagation
        .get("owner")
        .and_then(|value| value.as_str())
        .map(str::trim)
        != Some(worker_id)
    {
        return None;
    }
    if propagation
        .get("assignment_kind")
        .and_then(|value| value.as_str())
        .map(str::trim)
        != Some("task")
    {
        return None;
    }

    let assigned_at = parse_rfc3339_utc(
        propagation
            .get("assigned_at")
            .and_then(|value| value.as_str()),
    )?;
    let progress_started_at = propagation
        .get("progress_started_at")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| parse_rfc3339_utc(Some(value)));
    let progress_started_by_matches = propagation
        .get("progress_started_by")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .is_none_or(|owner| owner == worker_id);
    if progress_started_by_matches
        && progress_started_at.is_some_and(|started_at| started_at >= assigned_at)
    {
        return None;
    }

    let age = now_utc.signed_duration_since(assigned_at);
    if age < chrono::Duration::zero() {
        return None;
    }
    age.to_std().ok()
}

fn parse_rfc3339_utc(value: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value?)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&chrono::Utc))
}

fn recover_assigned_workers_without_task_progress(
    ctx: &mut EventLoopCtx,
    brehon_root: &std::path::Path,
    tasks_snapshot: &mut Vec<TaskInfo>,
    sessions_snapshot: &mut std::collections::HashMap<String, (String, String, String)>,
    now: std::time::Instant,
) {
    let now_utc = chrono::Utc::now();
    let candidates = tasks_snapshot
        .iter()
        .filter_map(|task| {
            let worker_id = task.assignee.as_deref()?;
            if !active_worker_task(task, worker_id) {
                return None;
            }
            let pane = ctx.mux.get(worker_id)?;
            if *pane.kind() != PaneKind::Worker
                || pane.has_exited()
                || pane.is_tool_executing()
                || matches!(
                    pane.pane_state(),
                    Some(
                        PaneState::Busy { .. } | PaneState::Blocked { .. } | PaneState::Dead { .. }
                    )
                )
            {
                return None;
            }
            let assignment_age =
                task_assignment_without_progress_age(brehon_root, task, worker_id, now_utc)?;
            (assignment_age >= ctx.auto_recover_threshold).then(|| {
                (
                    worker_id.to_string(),
                    task.clone(),
                    (assignment_age.as_secs() / 60).max(1),
                )
            })
        })
        .collect::<Vec<_>>();

    for (worker_id, task, assigned_mins) in candidates {
        if agent_is_quarantined_for_run(brehon_root, &worker_id)
            || worker_reset_or_recycle_command_pending(ctx, &worker_id)
        {
            continue;
        }
        let key = active_worker_recovery_key(&worker_id, &task.id);
        if ctx.active_worker_recovery_resets_sent.contains_key(&key) {
            continue;
        }
        match ctx.active_worker_recovery_nudges_sent.get(&key).copied() {
            Some(sent_at) if now.duration_since(sent_at) >= ctx.auto_recover_threshold => {}
            Some(_) => continue,
            None => {
                send_no_task_progress_recovery_nudge(ctx, &worker_id, &task, assigned_mins, now);
                continue;
            }
        }

        let live_sessions =
            worker_sessions_available_for_auto_recovery(ctx, brehon_root, sessions_snapshot);
        if let Some(outcome) = attempt_auto_recover_stalled_worker(
            brehon_root,
            &worker_id,
            tasks_snapshot,
            &live_sessions,
            assigned_mins,
        ) {
            if handle_auto_recovered_stalled_worker(
                ctx,
                brehon_root,
                &task,
                tasks_snapshot,
                sessions_snapshot,
                assigned_mins,
                now,
                outcome,
            ) {
                continue;
            }
        }
        reset_active_assigned_worker(ctx, &worker_id, &task.id, assigned_mins, false, now);
    }
}

pub(crate) fn build_dirty_worktree_handoff_nudge_message(
    task: &TaskInfo,
    task_cmd: &str,
    reason: &str,
    idle_secs: u64,
) -> String {
    format!(
        "[brehon] Dirty worktree handoff required for {task_id}: {title}\n\n\
         Brehon sees uncommitted work in your assigned worktree ({reason}) and the task is still \
         status `{status}` after {idle_secs}s idle. Shell output, prose, and local edits do not \
         update Brehon state. Make exactly one Brehon task call now:\n\n\
         - Ready for review: `{task_cmd} action=complete id={task_id} notes=\"<summary>\" activity=testing`\n\
         - Still working but need to preserve this revision: `{task_cmd} action=checkpoint id={task_id} message=\"<summary>\"`, then `{task_cmd} action=progress id={task_id} percent=<n> notes=\"<status>\" activity=<reading|editing|testing|reviewing>`\n\
         - Blocked: `{task_cmd} action=update id={task_id} status=blocked blockers=\"<reason>\"`\n\n\
         Do not run more edits first. Do not narrate completion. Call the Brehon MCP tool.",
        task_id = task.id,
        title = task.title,
        status = task.status,
    )
}

fn reset_active_assigned_worker(
    ctx: &mut EventLoopCtx,
    worker_id: &str,
    task_id: &str,
    idle_mins: u64,
    pane_dead: bool,
    now: std::time::Instant,
) -> bool {
    let key = active_worker_recovery_key(worker_id, task_id);
    if ctx.active_worker_recovery_resets_sent.contains_key(&key) {
        return false;
    }

    let reset_via = if ctx.runtime_agent_factory_host_owned {
        "via terminal-host reset"
    } else {
        "via authoritative reset"
    };
    let daemon_reason = if pane_dead {
        "auto-recover dead assigned worker pane via daemon reset"
    } else {
        "auto-recover idle assigned worker pane via daemon reset"
    };
    let success_message = if pane_dead {
        format!(
            "reset assigned worker {} for {} after pane exit {}",
            worker_id, task_id, reset_via
        )
    } else {
        format!(
            "reset assigned worker {} for {} after {}m idle {}",
            worker_id, task_id, idle_mins, reset_via
        )
    };
    if queue_worker_context_reset(
        ctx,
        worker_id,
        daemon_reason.to_string(),
        success_message.clone(),
        if pane_dead {
            format!("failed to reset dead assigned worker {worker_id} for {task_id}")
        } else {
            format!("failed to reset idle assigned worker {worker_id} for {task_id}")
        },
        now,
    ) {
        ctx.active_worker_recovery_resets_sent.insert(key, now);
        return true;
    }

    let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, worker_id) {
        build_worker_context_reset_startup_prompt(&ctx.mux, worker_id)
    } else {
        None
    };
    let reset_result = if ctx.runtime_agent_factory_host_owned {
        super::prompt_delivery::reset_terminal_host_pane(ctx, worker_id, daemon_reason)
    } else {
        ctx.rt
            .block_on(ctx.mux.reset_worker_gateway_session(worker_id))
            .map_err(|err| err.to_string())
    };
    match reset_result {
        Ok(()) => {
            if let Some(startup_prompt) = startup_prompt {
                if ctx.runtime_agent_factory_host_owned {
                    if let Err(err) = super::prompt_delivery::enqueue_terminal_host_startup_prompt(
                        ctx,
                        worker_id,
                        startup_prompt,
                        "terminal-host assigned worker reset startup prompt",
                    ) {
                        tracing::warn!(
                            worker = %worker_id,
                            task_id = %task_id,
                            error = %err,
                            "Failed to queue assigned worker reset startup prompt"
                        );
                    }
                } else {
                    ctx.mux.queue_startup_prompt(worker_id, startup_prompt);
                }
            }
            ctx.last_activity.insert(worker_id.to_string(), now);
            ctx.last_worker_context_reset
                .insert(worker_id.to_string(), now);
            ctx.pending_self_improve_prompt.remove(worker_id);
            ctx.active_worker_recovery_resets_sent.insert(key, now);
            ctx.needs_redraw = true;
            push_dashboard_event(&ctx.dashboard_data, success_message.clone());
            tracing::warn!(worker = %worker_id, task_id = %task_id, "{success_message}");
            true
        }
        Err(err) => {
            tracing::warn!(
                worker = %worker_id,
                task_id = %task_id,
                error = %err,
                "Failed to reset stale assigned worker"
            );
            false
        }
    }
}

fn refresh_stall_recovery_snapshot(
    ctx: &mut EventLoopCtx,
    brehon_root: &std::path::Path,
    tasks_snapshot: &mut Vec<TaskInfo>,
    sessions_snapshot: &mut std::collections::HashMap<String, (String, String, String)>,
) {
    *tasks_snapshot = read_task_files(brehon_root);
    *sessions_snapshot = read_session_files(brehon_root);
    sync_worker_task_contexts(&mut ctx.mux, tasks_snapshot, sessions_snapshot);
    ctx.dashboard_data.lock().unwrap().tasks = tasks_snapshot.clone();
    ctx.needs_redraw = true;
}

fn fence_recovered_worker_pane(
    ctx: &mut EventLoopCtx,
    worker_id: &str,
    task_id: &str,
    idle_mins: u64,
    now: std::time::Instant,
) -> bool {
    if ctx.mux.get(worker_id).is_none() || worker_reset_or_recycle_command_pending(ctx, worker_id) {
        return true;
    }

    let recycle_via = if ctx.runtime_agent_factory_host_owned {
        "via terminal-host recycle"
    } else {
        "via authoritative recycle"
    };
    let recycle_reason =
        format!("auto-fence recovered stalled worker pane after task {task_id} handoff");
    let success_message = format!(
        "recycled recovered worker {worker_id} after task {task_id} handoff following {idle_mins}m idle {recycle_via}"
    );

    if queue_worker_recycle(
        ctx,
        worker_id,
        None,
        None,
        recycle_reason.clone(),
        success_message.clone(),
        format!("failed to recycle recovered worker {worker_id} after task handoff"),
        now,
    ) {
        ctx.mux.clear_pane_task_context(worker_id);
        return true;
    }

    let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, worker_id) {
        build_worker_recycle_startup_prompt(&ctx.mux, worker_id)
    } else {
        None
    };
    let recycle_result = if ctx.runtime_agent_factory_host_owned {
        recycle_terminal_host_pane(ctx, worker_id, recycle_reason.clone())
    } else {
        let generation = ctx.rt.block_on(ctx.mux.recycle(worker_id, &recycle_reason));
        (generation.0 > 0)
            .then_some(())
            .ok_or_else(|| "authoritative recycle failed".to_string())
    };
    match recycle_result {
        Ok(()) => {
            ctx.mux.clear_pane_task_context(worker_id);
            if let Some(startup_prompt) = startup_prompt {
                if ctx.runtime_agent_factory_host_owned {
                    if let Err(err) = super::prompt_delivery::enqueue_terminal_host_startup_prompt(
                        ctx,
                        worker_id,
                        startup_prompt,
                        "terminal-host recovered worker recycle startup prompt",
                    ) {
                        tracing::warn!(
                            worker = %worker_id,
                            task_id = %task_id,
                            error = %err,
                            "Failed to queue recovered worker recycle startup prompt"
                        );
                    }
                } else {
                    ctx.mux.queue_startup_prompt(worker_id, startup_prompt);
                }
            }
            ctx.last_activity.insert(worker_id.to_string(), now);
            ctx.pending_self_improve_prompt.remove(worker_id);
            ctx.needs_redraw = true;
            push_dashboard_event(&ctx.dashboard_data, success_message.clone());
            tracing::warn!(worker = %worker_id, task_id = %task_id, "{success_message}");
            true
        }
        Err(err) => {
            tracing::warn!(
                worker = %worker_id,
                task_id = %task_id,
                error = %err,
                "Failed to fence recovered worker after task handoff"
            );
            false
        }
    }
}

#[must_use = "return false to fall through to reset_active_assigned_worker"]
fn handle_auto_recovered_stalled_worker(
    ctx: &mut EventLoopCtx,
    brehon_root: &std::path::Path,
    task: &TaskInfo,
    tasks_snapshot: &mut Vec<TaskInfo>,
    sessions_snapshot: &mut std::collections::HashMap<String, (String, String, String)>,
    idle_mins: u64,
    now: std::time::Instant,
    outcome: StalledRecoveryOutcome,
) -> bool {
    match outcome {
        StalledRecoveryOutcome::Reassigned {
            task_id,
            old_worker,
            new_worker,
        } => {
            refresh_stall_recovery_snapshot(ctx, brehon_root, tasks_snapshot, sessions_snapshot);
            ctx.last_activity.insert(new_worker.clone(), now);
            if !fence_recovered_worker_pane(ctx, &old_worker, &task_id, idle_mins, now) {
                return false;
            }
            let event = format!(
                "reassigned stalled task {task_id} from {old_worker} to {new_worker} after {idle_mins}m idle"
            );
            tracing::warn!(
                task_id = %task_id,
                old_worker = %old_worker,
                new_worker = %new_worker,
                idle_minutes = idle_mins,
                "{event}"
            );
            push_dashboard_event(&ctx.dashboard_data, event);
            let task_cmd = ctx
                .mux
                .get(&new_worker)
                .map(|pane| format!("{}task", pane.cli_type().capabilities().tool_prefix));
            if let Some(task_cmd) = task_cmd {
                let prompt =
                    build_recovered_task_assignment_prompt(task, &old_worker, idle_mins, &task_cmd);
                if !dispatch_runtime_prompt(ctx, &new_worker, prompt, None) {
                    let warning = format!(
                        "automatic reassignment delivered task {task_id} to {new_worker}, but the recovery assignment prompt could not be delivered"
                    );
                    tracing::warn!(
                        task_id = %task_id,
                        old_worker = %old_worker,
                        new_worker = %new_worker,
                        "{warning}"
                    );
                    push_dashboard_event(&ctx.dashboard_data, warning);
                }
            } else {
                tracing::debug!(
                    worker = %new_worker,
                    task_id = %task_id,
                    "pane not found after recovery; skipping assignment prompt"
                );
            }
            true
        }
        StalledRecoveryOutcome::Requeued {
            task_id,
            old_worker,
        } => {
            refresh_stall_recovery_snapshot(ctx, brehon_root, tasks_snapshot, sessions_snapshot);
            if !fence_recovered_worker_pane(ctx, &old_worker, &task_id, idle_mins, now) {
                return false;
            }
            let event = format!(
                "requeued stalled task {task_id} after {idle_mins}m idle; previous worker was {old_worker}"
            );
            tracing::warn!(
                task_id = %task_id,
                old_worker = %old_worker,
                idle_minutes = idle_mins,
                "{event}"
            );
            push_dashboard_event(&ctx.dashboard_data, event);
            true
        }
        StalledRecoveryOutcome::ReviewReady { task_id, worker } => {
            refresh_stall_recovery_snapshot(ctx, brehon_root, tasks_snapshot, sessions_snapshot);
            // ReviewReady preserves worker ownership — do not fence/recycle the pane.
            let event = format!(
                "moved stalled task {task_id} to review_ready after {idle_mins}m idle; preserving worker ownership for {worker}"
            );
            tracing::warn!(
                task_id = %task_id,
                worker = %worker,
                idle_minutes = idle_mins,
                "{event}"
            );
            push_dashboard_event(&ctx.dashboard_data, event);
            true
        }
        StalledRecoveryOutcome::SupervisorConflict {
            task_id,
            worker,
            files,
        } => {
            refresh_stall_recovery_snapshot(ctx, brehon_root, tasks_snapshot, sessions_snapshot);
            if !fence_recovered_worker_pane(ctx, &worker, &task_id, idle_mins, now) {
                return false;
            }
            let file_list = if files.is_empty() {
                "unknown files".to_string()
            } else {
                files.join(", ")
            };
            let event = format!(
                "escalated stalled task {task_id} to supervisor-owned integration_conflict after {idle_mins}m idle; previous worker {worker}; conflicting files: {file_list}"
            );
            tracing::warn!(
                task_id = %task_id,
                worker = %worker,
                idle_minutes = idle_mins,
                conflicting_files = %file_list,
                "{event}"
            );
            push_dashboard_event(&ctx.dashboard_data, event);
            true
        }
        StalledRecoveryOutcome::Blocked {
            task_id,
            worker,
            reason,
        } => {
            let event = format!(
                "stalled task {task_id} stayed with worker {worker}; safe reassignment was blocked because {reason}"
            );
            tracing::warn!(
                task_id = %task_id,
                worker = %worker,
                idle_minutes = idle_mins,
                reason = %reason,
                "{event}"
            );
            push_dashboard_event(&ctx.dashboard_data, event);
            if send_dirty_worktree_handoff_nudge(
                ctx,
                &worker,
                task,
                &reason,
                idle_mins.saturating_mul(60),
                now,
            ) {
                return true;
            }
            match block_task_for_stalled_worker_manual_recovery(
                brehon_root,
                &task_id,
                &worker,
                &reason,
            ) {
                Ok(()) => {
                    refresh_stall_recovery_snapshot(
                        ctx,
                        brehon_root,
                        tasks_snapshot,
                        sessions_snapshot,
                    );
                    let event = format!(
                        "blocked stalled task {task_id} for supervisor/manual recovery; dirty worker {worker} could not be safely nudged or reassigned: {reason}"
                    );
                    tracing::warn!(
                        task_id = %task_id,
                        worker = %worker,
                        idle_minutes = idle_mins,
                        reason = %reason,
                        activity = STALLED_WORKER_MANUAL_RECOVERY_ACTIVITY,
                        "{event}"
                    );
                    push_dashboard_event(&ctx.dashboard_data, event);
                }
                Err(err) => {
                    let event = format!(
                        "failed to block dirty stalled task {task_id} for supervisor/manual recovery: {err}"
                    );
                    tracing::warn!(
                        task_id = %task_id,
                        worker = %worker,
                        idle_minutes = idle_mins,
                        reason = %reason,
                        error = %err,
                        "{event}"
                    );
                    push_dashboard_event(&ctx.dashboard_data, event);
                }
            }
            true
        }
        StalledRecoveryOutcome::ManualRecoveryRequired {
            task_id,
            worker,
            reason,
        } => match block_task_for_stalled_worker_manual_recovery(
            brehon_root,
            &task_id,
            &worker,
            &reason,
        ) {
            Ok(()) => {
                refresh_stall_recovery_snapshot(
                    ctx,
                    brehon_root,
                    tasks_snapshot,
                    sessions_snapshot,
                );
                let event = format!(
                    "blocked stalled task {task_id} for supervisor/manual recovery after {idle_mins}m idle; previous worker {worker}; reason: {reason}"
                );
                tracing::warn!(
                    task_id = %task_id,
                    worker = %worker,
                    idle_minutes = idle_mins,
                    reason = %reason,
                    activity = STALLED_WORKER_MANUAL_RECOVERY_ACTIVITY,
                    "{event}"
                );
                push_dashboard_event(&ctx.dashboard_data, event);
                true
            }
            Err(err) => {
                let event = format!(
                    "failed to persist manual stalled-worker recovery block for task {task_id}: {err}"
                );
                tracing::warn!(
                    task_id = %task_id,
                    worker = %worker,
                    idle_minutes = idle_mins,
                    reason = %reason,
                    error = %err,
                    "{event}"
                );
                push_dashboard_event(&ctx.dashboard_data, event);
                true
            }
        },
    }
}

fn prune_active_worker_recovery_records(ctx: &mut EventLoopCtx, tasks: &[TaskInfo]) {
    let active_keys: std::collections::HashSet<(String, String)> = tasks
        .iter()
        .filter(|task| task.task_type == "task")
        .filter_map(|task| {
            let worker_id = task.assignee.as_deref()?;
            active_worker_task(task, worker_id)
                .then(|| active_worker_recovery_key(worker_id, &task.id))
        })
        .collect();
    ctx.active_worker_recovery_nudges_sent
        .retain(|key, _| active_keys.contains(key));
    ctx.active_worker_recovery_resets_sent
        .retain(|key, _| active_keys.contains(key));
}

pub(super) fn detect_and_handle_stalls(ctx: &mut EventLoopCtx) {
    if ctx.last_stall_check.elapsed() < ctx.stall_check_interval {
        return;
    }
    ctx.last_stall_check = std::time::Instant::now();
    let now = std::time::Instant::now();

    for pane in ctx.mux.panes() {
        ctx.last_activity
            .entry(pane.id().to_string())
            .or_insert(now);
    }

    let brehon_root = ctx.dashboard_data.lock().unwrap().brehon_root.clone();
    let (mut tasks_snapshot, mut sessions_snapshot) = if let Some(ref root) = brehon_root {
        (read_task_files(root), read_session_files(root))
    } else {
        (Vec::new(), std::collections::HashMap::new())
    };

    if let Some(root) = brehon_root.as_ref() {
        for pane_id in prompt_blocked_agent_names(ctx, root, &tasks_snapshot, &sessions_snapshot) {
            let live_pane_kind = ctx.mux.get(&pane_id).map(|pane| pane.kind().clone());
            let pane_kind = live_pane_kind
                .clone()
                .or_else(|| prompt_blocked_session_pane_kind(&sessions_snapshot, &pane_id));
            if pane_kind
                .as_ref()
                .is_some_and(|kind| !prompt_blocked_recoverable_pane_kind(kind))
            {
                continue;
            }
            let detail = prompt_blocked_marker_detail(root, &pane_id)
                .or_else(|| prompt_blocked_pane_detail(ctx, &pane_id))
                .unwrap_or_else(|| "prompt-blocked terminal approval request".to_string());
            let owning_task_id = prompt_blocked_owned_task_id(ctx, root, &pane_id);
            let recovery = if live_pane_kind.is_none() {
                let agent_label = pane_kind.as_ref().map(PaneKind::as_str).unwrap_or("agent");
                recover_prompt_blocked_missing_pane(
                    ctx,
                    root,
                    &pane_id,
                    agent_label,
                    owning_task_id.as_deref(),
                    &detail,
                )
            } else if pane_kind.as_ref() == Some(&PaneKind::Worker) {
                let active_task_id = prompt_blocked_active_task_id(ctx, &tasks_snapshot, &pane_id);
                recover_prompt_blocked_worker(
                    ctx,
                    root,
                    &pane_id,
                    active_task_id.as_deref(),
                    owning_task_id.as_deref(),
                    &detail,
                    now,
                )
            } else {
                reset_prompt_blocked_non_worker(
                    ctx,
                    root,
                    &pane_id,
                    pane_kind.expect("live prompt-blocked pane kind"),
                    owning_task_id.as_deref(),
                    &detail,
                    now,
                )
            };
            if recovery.task_files_modified() {
                tasks_snapshot = read_task_files(root);
                ctx.dashboard_data.lock().unwrap().tasks = tasks_snapshot.clone();
            }
        }

        const SUPERVISOR_QUARANTINE_RESET_COOLDOWN: std::time::Duration =
            std::time::Duration::from_secs(60);
        for supervisor_id in quarantined_supervisor_names(root, &sessions_snapshot) {
            let Some(pane) = ctx.mux.get(&supervisor_id) else {
                continue;
            };
            if pane.kind() != &PaneKind::Supervisor {
                continue;
            }
            if agent_health_marker_reason(root, &supervisor_id).as_deref() == Some("prompt_blocked")
                && matches!(pane.pane_state(), Some(PaneState::Ready { .. }))
            {
                clear_agent_health_marker(root, &supervisor_id);
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!(
                        "cleared stale prompt-blocked marker for ready supervisor {supervisor_id}"
                    ),
                );
                continue;
            }
            if ctx
                .last_supervisor_reset
                .get(&supervisor_id)
                .is_some_and(|last| {
                    now.duration_since(*last) < SUPERVISOR_QUARANTINE_RESET_COOLDOWN
                })
            {
                continue;
            }
            let reset_via = if ctx.runtime_agent_factory_host_owned {
                "via terminal-host reset"
            } else {
                "via authoritative reset"
            };
            if queue_supervisor_context_reset(
                ctx,
                &supervisor_id,
                "auto-recover quarantined supervisor pane via daemon reset".to_string(),
                format!("reset quarantined supervisor {supervisor_id} {reset_via}"),
                format!("failed to reset quarantined supervisor {supervisor_id}"),
                now,
            ) {
                continue;
            }
            let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, &supervisor_id) {
                build_supervisor_reset_startup_prompt(
                    &ctx.mux,
                    &supervisor_id,
                    ctx.runtime_agent_factory_host_owned,
                )
            } else {
                None
            };
            let reset_summary = if ctx.runtime_agent_factory_host_owned {
                match reset_terminal_host_pane(
                    ctx,
                    &supervisor_id,
                    "auto-recover quarantined supervisor pane via terminal-host reset",
                ) {
                    Ok(()) => {
                        clear_agent_health_marker(root, &supervisor_id);
                        if let Some(startup_prompt) = startup_prompt {
                            if let Err(err) =
                                super::prompt_delivery::enqueue_terminal_host_startup_prompt(
                                    ctx,
                                    &supervisor_id,
                                    startup_prompt,
                                    "terminal-host supervisor quarantine reset startup prompt",
                                )
                            {
                                tracing::warn!(
                                    supervisor = %supervisor_id,
                                    error = %err,
                                    "Failed to queue terminal-host supervisor quarantine reset startup prompt"
                                );
                            }
                        }
                        "via terminal-host reset".to_string()
                    }
                    Err(err) => {
                        tracing::warn!(
                            supervisor = %supervisor_id,
                            error = %err,
                            "Failed to reset quarantined host-owned supervisor"
                        );
                        continue;
                    }
                }
            } else {
                let reset_result = ctx
                    .rt
                    .block_on(ctx.mux.reset_supervisor_session(&supervisor_id));
                match reset_result {
                    Ok(()) => {
                        clear_agent_health_marker(root, &supervisor_id);
                        if let Some(startup_prompt) = startup_prompt {
                            ctx.mux.queue_startup_prompt(&supervisor_id, startup_prompt);
                        }
                        "via authoritative reset".to_string()
                    }
                    Err(err) => {
                        tracing::warn!(
                            supervisor = %supervisor_id,
                            error = %err,
                            "Failed to reset quarantined supervisor"
                        );
                        continue;
                    }
                }
            };
            ctx.last_activity.insert(supervisor_id.clone(), now);
            ctx.last_supervisor_reset.insert(supervisor_id.clone(), now);
            ctx.pending_self_improve_prompt.remove(&supervisor_id);
            push_dashboard_event(
                &ctx.dashboard_data,
                format!("reset quarantined supervisor {supervisor_id} {reset_summary}"),
            );
        }

        for worker_id in quarantined_worker_names(root, &tasks_snapshot, &sessions_snapshot) {
            if ctx
                .mux
                .get(&worker_id)
                .is_none_or(|pane| *pane.kind() != PaneKind::Worker)
            {
                continue;
            }
            let reset_via = if ctx.runtime_agent_factory_host_owned {
                "via terminal-host reset"
            } else {
                "via authoritative reset"
            };
            if queue_worker_context_reset(
                ctx,
                &worker_id,
                "auto-recover quarantined worker pane via daemon reset".to_string(),
                format!("reset quarantined worker {} {}", worker_id, reset_via),
                format!("failed to reset quarantined worker {worker_id}"),
                now,
            ) {
                tasks_snapshot = read_task_files(root);
                ctx.dashboard_data.lock().unwrap().tasks = tasks_snapshot.clone();
                continue;
            }
            let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, &worker_id) {
                build_worker_context_reset_startup_prompt(&ctx.mux, &worker_id)
            } else {
                None
            };
            let reset_summary = if ctx.runtime_agent_factory_host_owned {
                match super::prompt_delivery::reset_terminal_host_pane(
                    ctx,
                    &worker_id,
                    "auto-recover quarantined worker pane via terminal-host reset",
                ) {
                    Ok(()) => {
                        clear_agent_health_marker(root, &worker_id);
                        if let Some(startup_prompt) = startup_prompt {
                            if let Err(err) =
                                super::prompt_delivery::enqueue_terminal_host_startup_prompt(
                                    ctx,
                                    &worker_id,
                                    startup_prompt,
                                    "terminal-host worker quarantine reset startup prompt",
                                )
                            {
                                tracing::warn!(
                                    worker = %worker_id,
                                    error = %err,
                                    "Failed to queue terminal-host worker quarantine reset startup prompt"
                                );
                            }
                        }
                        "via terminal-host reset".to_string()
                    }
                    Err(err) => {
                        tracing::warn!(
                            worker = %worker_id,
                            error = %err,
                            "Failed to reset quarantined host-owned worker"
                        );
                        continue;
                    }
                }
            } else {
                let reset_result = ctx
                    .rt
                    .block_on(ctx.mux.reset_worker_gateway_session(&worker_id));
                match reset_result {
                    Ok(()) => {
                        clear_agent_health_marker(root, &worker_id);
                        if let Some(startup_prompt) = startup_prompt {
                            ctx.mux.queue_startup_prompt(&worker_id, startup_prompt);
                        }
                        "via authoritative reset".to_string()
                    }
                    Err(err) => {
                        tracing::warn!(
                            worker = %worker_id,
                            error = %err,
                            "Failed to reset quarantined worker"
                        );
                        continue;
                    }
                }
            };
            ctx.last_activity.insert(worker_id.clone(), now);
            ctx.last_worker_context_reset.insert(worker_id.clone(), now);
            ctx.pending_self_improve_prompt.remove(&worker_id);
            push_dashboard_event(
                &ctx.dashboard_data,
                format!("reset quarantined worker {} {}", worker_id, reset_summary),
            );
            tasks_snapshot = read_task_files(root);
            ctx.dashboard_data.lock().unwrap().tasks = tasks_snapshot.clone();
        }
    }

    if let Some(supervisor_pane_id) = ctx.supervisor_id.clone() {
        let frontier = compute_supervisor_dispatch_frontier(&tasks_snapshot, &sessions_snapshot);
        if frontier.is_none() {
            ctx.last_supervisor_dispatch_nudge = None;
        } else if let Some(frontier) = frontier {
            let supervisor_idle = now
                .checked_duration_since(
                    ctx.last_activity
                        .get(&supervisor_pane_id)
                        .copied()
                        .unwrap_or(std::time::Instant::now()),
                )
                .unwrap_or_default();
            let should_nudge = ctx
                .mux
                .get(&supervisor_pane_id)
                .filter(|pane| !pane.is_tool_executing())
                .is_some()
                && supervisor_idle >= ctx.supervisor_dispatch_nudge_quiet_threshold
                && match &ctx.last_supervisor_dispatch_nudge {
                    Some((signature, sent_at)) => {
                        signature != &frontier.signature()
                            || now.duration_since(*sent_at)
                                >= ctx.supervisor_dispatch_nudge_cooldown
                    }
                    None => true,
                };
            if should_nudge {
                let signature = frontier.signature();
                let message = build_supervisor_dispatch_nudge_message(
                    &frontier,
                    ctx.runtime_agent_factory_host_owned,
                );
                dispatch_runtime_prompt(ctx, &supervisor_pane_id, message, None);
                ctx.last_supervisor_dispatch_nudge = Some((signature, now));
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!(
                        "supervisor nudged: {} idle worker(s), {} conflicts, {} pending, {} changes_requested, {} review_ready, {} approved",
                        frontier.idle_workers.len(),
                        frontier.integration_conflict_tasks.len(),
                        frontier.pending_tasks.len(),
                        frontier.changes_requested_tasks.len(),
                        frontier.review_ready_tasks.len(),
                        frontier.approved_tasks.len(),
                    ),
                );
            }
        }
    }

    if let Some(root) = brehon_root.as_ref() {
        let pending_review_obligations = read_pending_review_obligations(root, &tasks_snapshot);
        recover_stale_reviewer_obligations(ctx, root, &pending_review_obligations, now);
        prune_review_obligation_tracking_records(ctx, &pending_review_obligations);
    }

    // ── Post-checkpoint handoff nudge ───────────────────────────────────────
    //
    // A worker whose task is still `in_progress` but whose `latest_commit`
    // is set has checkpointed. If they've been idle past the threshold and
    // we haven't already nudged them for this commit, remind them to call
    // `action=complete` or `action=progress` — the protocol failure mode
    // this rescues is a silent deadlock where the worker believes they've
    // handed off but the task is still waiting on them, eventually losing
    // their work to the 15-minute authoritative-recycle.
    //
    // Runs BEFORE the recycle pass below so the nudge gets a chance to
    // land on an already-running worker instead of one that just got
    // force-restarted. Nudges are keyed by (worker, task, commit) so the
    // same checkpoint can't produce duplicate nudges, and a fresh
    // checkpoint (new commit SHA) legitimately earns a new nudge window.
    send_post_checkpoint_handoff_nudges(ctx, &tasks_snapshot, now);
    if let Some(root) = brehon_root.as_ref() {
        recover_assigned_workers_without_task_progress(
            ctx,
            root,
            &mut tasks_snapshot,
            &mut sessions_snapshot,
            now,
        );
        send_dirty_worktree_handoff_nudges(ctx, root, &tasks_snapshot, now);
    }

    let stale_worker_candidates: Vec<(String, bool)> = ctx
        .mux
        .panes()
        .filter_map(|pane| {
            if *pane.kind() != PaneKind::Worker {
                return None;
            }
            let pane_dead =
                pane.has_exited() || matches!(pane.pane_state(), Some(PaneState::Dead { .. }));
            let pane_busy = matches!(pane.pane_state(), Some(PaneState::Busy { .. }));
            if !pane_dead && (pane_busy || pane.is_tool_executing()) {
                return None;
            }
            let pane_id = pane.id().to_string();
            let worker_idle = now
                .checked_duration_since(
                    ctx.last_activity
                        .get(&pane_id)
                        .copied()
                        .unwrap_or(std::time::Instant::now()),
                )
                .unwrap_or_default();
            (pane_dead || worker_idle >= ctx.auto_recover_threshold).then_some((pane_id, pane_dead))
        })
        .collect();

    for (pane_id, pane_dead) in stale_worker_candidates {
        if brehon_root
            .as_ref()
            .is_some_and(|root| agent_is_quarantined_for_run(root, &pane_id))
        {
            continue;
        }
        if worker_reset_or_recycle_command_pending(ctx, &pane_id) {
            continue;
        }
        let Some(pane) = ctx.mux.get(&pane_id) else {
            continue;
        };
        let pane_dead = pane_dead
            || pane.has_exited()
            || matches!(pane.pane_state(), Some(PaneState::Dead { .. }));
        if !pane_dead
            && (pane.is_tool_executing()
                || matches!(pane.pane_state(), Some(PaneState::Busy { .. })))
        {
            continue;
        }
        let worker_idle = current_worker_idle_duration(ctx, &pane_id, now);
        if !pane_dead && worker_idle < ctx.auto_recover_threshold {
            continue;
        }
        let idle_mins = (worker_idle.as_secs() / 60).max(1);
        if let Some(task) = active_assigned_task_for_worker(&tasks_snapshot, &pane_id).cloned() {
            let key = active_worker_recovery_key(&pane_id, &task.id);
            if ctx.active_worker_recovery_resets_sent.contains_key(&key) {
                continue;
            }
            if !pane_dead {
                match ctx.active_worker_recovery_nudges_sent.get(&key).copied() {
                    Some(sent_at) if now.duration_since(sent_at) >= ctx.auto_recover_threshold => {}
                    Some(_) => continue,
                    None => {
                        send_active_worker_recovery_nudge(ctx, &pane_id, &task, idle_mins, now);
                        continue;
                    }
                }
            }
            if let Some(root) = brehon_root.as_ref() {
                let live_sessions =
                    worker_sessions_available_for_auto_recovery(ctx, root, &sessions_snapshot);
                if let Some(outcome) = attempt_auto_recover_stalled_worker(
                    root,
                    &pane_id,
                    &tasks_snapshot,
                    &live_sessions,
                    idle_mins,
                ) {
                    if handle_auto_recovered_stalled_worker(
                        ctx,
                        root,
                        &task,
                        &mut tasks_snapshot,
                        &mut sessions_snapshot,
                        idle_mins,
                        now,
                        outcome,
                    ) {
                        continue;
                    }
                }
            }
            reset_active_assigned_worker(ctx, &pane_id, &task.id, idle_mins, pane_dead, now);
            continue;
        }
        let recycle_via = if ctx.runtime_agent_factory_host_owned {
            "via terminal-host recycle"
        } else {
            "via authoritative recycle"
        };
        let daemon_reason = if pane_dead {
            "auto-recover dead worker pane via daemon recycle"
        } else {
            "auto-recover idle worker pane via daemon recycle"
        };
        let success_message = if pane_dead {
            format!(
                "recycled worker {} after pane exit {}",
                pane_id, recycle_via
            )
        } else {
            format!(
                "recycled worker {} after {}m idle {}",
                pane_id, idle_mins, recycle_via
            )
        };
        if queue_worker_recycle(
            ctx,
            &pane_id,
            None,
            None,
            daemon_reason.to_string(),
            success_message,
            if pane_dead {
                format!("failed to recycle dead worker {pane_id}")
            } else {
                format!("failed to recycle idle worker {pane_id}")
            },
            now,
        ) {
            continue;
        }
        let recycle_summary = if ctx.runtime_agent_factory_host_owned {
            let terminal_host_reason = if pane_dead {
                "auto-recover dead worker pane via terminal-host recycle"
            } else {
                "auto-recover idle worker pane via terminal-host recycle"
            };
            match recycle_terminal_host_pane(ctx, &pane_id, terminal_host_reason) {
                Ok(()) => "via terminal-host recycle".to_string(),
                Err(err) => {
                    tracing::warn!(
                        worker = %pane_id,
                        error = %err,
                        "Failed to recycle idle host-owned worker"
                    );
                    continue;
                }
            }
        } else {
            let generation = ctx.rt.block_on(ctx.mux.recycle(
                &pane_id,
                "auto-recover idle worker pane via authoritative recycle",
            ));
            if generation.0 == 0 {
                continue;
            }
            format!("via authoritative recycle (generation {})", generation.0)
        };
        ctx.last_activity.insert(pane_id.clone(), now);
        ctx.pending_self_improve_prompt.remove(&pane_id);
        let event_message = if pane_dead {
            format!(
                "recycled worker {} after pane exit {}",
                pane_id, recycle_summary
            )
        } else {
            format!(
                "recycled worker {} after {}m idle {}",
                pane_id, idle_mins, recycle_summary
            )
        };
        push_dashboard_event(&ctx.dashboard_data, event_message);
    }

    prune_active_worker_recovery_records(ctx, &tasks_snapshot);

    if ctx.orchestration.worker_idle_behavior == WorkerIdleBehavior::SelfImprove
        && !ctx.orchestration.self_improve_tasks.is_empty()
    {
        let Some(root) = brehon_root.as_ref() else {
            return;
        };
        for worker_id in ctx.worker_ids.clone() {
            if agent_is_quarantined_for_run(root, &worker_id) {
                ctx.pending_self_improve_prompt.remove(&worker_id);
                continue;
            }
            let Some(pane) = ctx.mux.get(&worker_id) else {
                ctx.pending_self_improve_prompt.remove(&worker_id);
                continue;
            };
            if pane.is_tool_executing() {
                continue;
            }
            let Some(task) = find_review_wait_task_for_worker(&tasks_snapshot, &worker_id) else {
                ctx.pending_self_improve_prompt.remove(&worker_id);
                continue;
            };
            let Some(context) = pane.task_context() else {
                continue;
            };
            if context.task_id != task.id {
                continue;
            }
            let worker_idle = now
                .checked_duration_since(
                    ctx.last_activity
                        .get(&worker_id)
                        .copied()
                        .unwrap_or(std::time::Instant::now()),
                )
                .unwrap_or_default();
            if worker_idle < ctx.self_improve_idle_threshold {
                continue;
            }
            if let Some(sent_at) = ctx.pending_self_improve_prompt.get(&worker_id) {
                if now.duration_since(*sent_at) < ctx.self_improve_retry_cooldown {
                    continue;
                }
            }
            let start_index = ctx
                .next_self_improve_index
                .get(&worker_id)
                .copied()
                .unwrap_or(0);
            let Some((task_index, task_name, prompt)) = next_self_improvement_prompt(
                task,
                &ctx.orchestration.self_improve_tasks,
                ctx.orchestration.allow_mutating_idle_work,
                start_index,
            ) else {
                continue;
            };
            dispatch_runtime_prompt(ctx, &worker_id, prompt, None);
            ctx.pending_self_improve_prompt
                .insert(worker_id.clone(), now);
            ctx.last_activity.insert(worker_id.clone(), now);
            ctx.next_self_improve_index.insert(
                worker_id.clone(),
                (task_index + 1) % ctx.orchestration.self_improve_tasks.len(),
            );
            push_dashboard_event(
                &ctx.dashboard_data,
                format!(
                    "queued task-scoped self-improvement `{task_name}` for {worker_id} while {task_id} waits in {status}",
                    task_id = task.id,
                    status = task.status,
                ),
            );
        }
    }
}

/// Detect workers that have checkpointed a task and gone idle without
/// completing the handoff, and deliver a deterministic nudge prompt.
///
/// Matching criteria (all must hold):
/// * Task is `in_progress` and has a non-empty `latest_commit`.
/// * Task has an `assignee` that matches an active worker pane.
/// * That pane is not currently executing a tool.
/// * Pane has been idle for at least `post_checkpoint_nudge_threshold`.
/// * We haven't already nudged this (worker, task, commit) tuple within
///   `post_checkpoint_nudge_cooldown`.
///
/// Dedup key includes the commit SHA so a fresh checkpoint (distinct
/// commit) earns a new nudge window — which is desirable if the worker
/// ignores the first nudge and checkpoints again without completing.
pub(crate) fn send_post_checkpoint_handoff_nudges(
    ctx: &mut super::event_loop::EventLoopCtx,
    tasks: &[super::types::TaskInfo],
    now: std::time::Instant,
) {
    // Collect up-front the mux-dependent view so the pure candidate
    // function can be exercised under test without a Mux fixture.
    let worker_pane_ids: std::collections::HashSet<String> = ctx
        .mux
        .panes()
        .filter(|pane| {
            *pane.kind() == PaneKind::Worker
                && !pane.has_exited()
                && !matches!(pane.pane_state(), Some(PaneState::Dead { .. }))
        })
        .map(|pane| pane.id().to_string())
        .collect();
    let busy_worker_ids: std::collections::HashSet<String> = ctx
        .mux
        .panes()
        .filter(|pane| *pane.kind() == PaneKind::Worker && pane.is_tool_executing())
        .map(|pane| pane.id().to_string())
        .collect();

    let candidates = find_post_checkpoint_nudge_candidates(
        tasks,
        &worker_pane_ids,
        &busy_worker_ids,
        &ctx.last_activity,
        &ctx.post_checkpoint_nudges_sent,
        ctx.post_checkpoint_nudge_threshold,
        ctx.post_checkpoint_nudge_cooldown,
        now,
    );

    for candidate in candidates {
        let PostCheckpointNudgeCandidate {
            worker_id,
            task_id,
            commit,
            idle_secs,
        } = candidate;
        let short_commit = commit.chars().take(12).collect::<String>();
        let prompt = build_post_checkpoint_nudge_message(&task_id, &short_commit, idle_secs);
        dispatch_runtime_prompt(ctx, &worker_id, prompt, None);
        ctx.post_checkpoint_nudges_sent
            .insert((worker_id.clone(), task_id.clone(), commit.clone()), now);
        // Don't reset last_activity — if the nudge doesn't rouse the worker,
        // the existing idle-recycle still needs to fire on schedule.
        push_dashboard_event(
            &ctx.dashboard_data,
            format!(
                "nudged {worker_id} to complete or progress {task_id} (checkpointed at {short_commit}, idle {idle_secs}s)",
            ),
        );
    }

    prune_post_checkpoint_nudge_records(
        &mut ctx.post_checkpoint_nudges_sent,
        tasks,
        ctx.post_checkpoint_nudge_cooldown,
        now,
    );
}

/// Detect assigned workers that have uncommitted work in their dedicated
/// worktree but have not made a Brehon lifecycle call.
///
/// This covers the no-MCP handoff failure mode: the agent edited and tested
/// locally, then ended the turn with prose instead of `action=complete`,
/// `action=checkpoint`, or `action=progress`. A dirty assigned worktree must
/// not be reset; the worker needs an explicit handoff prompt first.
pub(crate) fn send_dirty_worktree_handoff_nudges(
    ctx: &mut super::event_loop::EventLoopCtx,
    brehon_root: &std::path::Path,
    tasks: &[super::types::TaskInfo],
    now: std::time::Instant,
) {
    let worker_panes: std::collections::HashSet<String> = ctx
        .mux
        .panes()
        .filter(|pane| {
            *pane.kind() == PaneKind::Worker
                && !pane.has_exited()
                && !matches!(pane.pane_state(), Some(PaneState::Dead { .. }))
        })
        .map(|pane| pane.id().to_string())
        .collect();
    let busy_workers: std::collections::HashSet<String> = ctx
        .mux
        .panes()
        .filter(|pane| {
            *pane.kind() == PaneKind::Worker
                && (pane.is_tool_executing()
                    || matches!(pane.pane_state(), Some(PaneState::Busy { .. })))
        })
        .map(|pane| pane.id().to_string())
        .collect();

    for task in tasks {
        let Some(worker_id) = task.assignee.as_deref() else {
            continue;
        };
        if !active_worker_task(task, worker_id)
            || !worker_panes.contains(worker_id)
            || busy_workers.contains(worker_id)
        {
            continue;
        }
        let worker_idle = now
            .checked_duration_since(ctx.last_activity.get(worker_id).copied().unwrap_or(now))
            .unwrap_or_default();
        if worker_idle < ctx.post_checkpoint_nudge_threshold {
            continue;
        }
        let key = active_worker_recovery_key(worker_id, &task.id);
        if ctx
            .active_worker_recovery_nudges_sent
            .get(&key)
            .is_some_and(|sent_at| {
                now.saturating_duration_since(*sent_at) < ctx.post_checkpoint_nudge_cooldown
            })
        {
            continue;
        }
        if let WorkerWorktreeInspection::Dirty(reason) =
            inspect_worker_worktree_state(brehon_root, worker_id)
        {
            send_dirty_worktree_handoff_nudge(
                ctx,
                worker_id,
                task,
                &reason,
                worker_idle.as_secs(),
                now,
            );
        }
    }
}

/// A worker+task+commit triple that is due a handoff-reminder nudge.
///
/// Exposed as a plain struct rather than a tuple so the field order at
/// call sites is unambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PostCheckpointNudgeCandidate {
    pub worker_id: String,
    pub task_id: String,
    pub commit: String,
    pub idle_secs: u64,
}

/// Pure predicate: given a task snapshot plus a view of worker pane state,
/// return every (worker, task, commit) triple that currently matches all
/// the handoff-nudge criteria. Extracted so the filtering logic is
/// testable without a live Mux/EventLoopCtx.
#[allow(clippy::too_many_arguments)]
pub(crate) fn find_post_checkpoint_nudge_candidates(
    tasks: &[super::types::TaskInfo],
    worker_pane_ids: &std::collections::HashSet<String>,
    busy_worker_ids: &std::collections::HashSet<String>,
    last_activity: &std::collections::HashMap<String, std::time::Instant>,
    nudges_sent: &std::collections::HashMap<(String, String, String), std::time::Instant>,
    idle_threshold: std::time::Duration,
    cooldown: std::time::Duration,
    now: std::time::Instant,
) -> Vec<PostCheckpointNudgeCandidate> {
    tasks
        .iter()
        .filter(|task| {
            matches!(
                brehon_types::task::normalize_task_status(&task.status),
                Some("in_progress")
            )
        })
        .filter_map(|task| {
            let commit = task
                .latest_commit
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())?;
            let assignee = task.assignee.as_deref()?;
            if !worker_pane_ids.contains(assignee) {
                return None;
            }
            if busy_worker_ids.contains(assignee) {
                return None;
            }
            let worker_idle =
                now.checked_duration_since(last_activity.get(assignee).copied().unwrap_or(now))?;
            if worker_idle < idle_threshold {
                return None;
            }
            // Skip if we've already nudged this commit within cooldown.
            let key = (assignee.to_string(), task.id.clone(), commit.to_string());
            if nudges_sent
                .get(&key)
                .is_some_and(|sent_at| now.saturating_duration_since(*sent_at) < cooldown)
            {
                return None;
            }
            Some(PostCheckpointNudgeCandidate {
                worker_id: assignee.to_string(),
                task_id: task.id.clone(),
                commit: commit.to_string(),
                idle_secs: worker_idle.as_secs(),
            })
        })
        .collect()
}

/// Drop nudge bookkeeping that's no longer actionable:
/// * cooldown has fully elapsed (the triple will be re-eligible by natural
///   re-entry if conditions still hold), or
/// * the task the nudge was for has moved past `in_progress`, been
///   reassigned, or checkpointed to a new commit.
pub(crate) fn prune_post_checkpoint_nudge_records(
    nudges_sent: &mut std::collections::HashMap<(String, String, String), std::time::Instant>,
    tasks: &[super::types::TaskInfo],
    cooldown: std::time::Duration,
    now: std::time::Instant,
) {
    nudges_sent.retain(|(_, task_id, commit), sent_at| {
        if now.saturating_duration_since(*sent_at) > cooldown {
            return false;
        }
        tasks.iter().any(|task| {
            task.id == *task_id
                && task.latest_commit.as_deref().map(str::trim) == Some(commit.as_str())
                && matches!(
                    brehon_types::task::normalize_task_status(&task.status),
                    Some("in_progress")
                )
        })
    });
}

/// Build the worker-facing reminder prompt. Kept as a free function so
/// it's unit-testable without a full mux fixture.
pub(crate) fn build_post_checkpoint_nudge_message(
    task_id: &str,
    short_commit: &str,
    idle_secs: u64,
) -> String {
    format!(
        "[brehon] Handoff check for {task_id}.\n\n\
         You recorded a checkpoint at commit {short_commit} and have been idle for {idle_secs}s. \
         Checkpoint does NOT transition the task — it only records a mid-task snapshot and leaves \
         status as `in_progress`. The supervisor is waiting for one of these two calls:\n\n\
         • If implementation is complete and ready for review:\n  \
           `task action=complete id={task_id} notes=\"<summary>\" activity=testing`\n  \
           (creates the handoff commit, moves task to `review_ready`, notifies supervisor)\n\n\
         • If you are still working:\n  \
           `task action=progress id={task_id} percent=<n> notes=\"<status>\" activity=<reading|editing|testing|reviewing>`\n\n\
         Pick one and call it now. Do NOT narrate a plan — just make the tool call."
    )
}

#[cfg(test)]
mod post_checkpoint_nudge_tests {
    use super::super::types::TaskInfo;
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::time::{Duration, Instant};

    fn in_progress_task(id: &str, assignee: &str, latest_commit: Option<&str>) -> TaskInfo {
        TaskInfo {
            id: id.to_string(),
            title: format!("{id} title"),
            status: "in_progress".to_string(),
            assignee: Some(assignee.to_string()),
            task_type: "task".to_string(),
            parent_id: None,
            description: String::new(),
            priority: None,
            percent: None,
            tokens_used: 0,
            completion_mode: None,
            merge_target: None,
            integration_status: None,
            integration_branch: None,
            integration_worktree: None,
            activity: None,
            notes: None,
            blockers: None,
            dependencies: Vec::new(),
            blocked_by: Vec::new(),
            created_at: None,
            updated_at: None,
            closed_at: None,
            closed_by: None,
            merged_commit: None,
            merged_branch: None,
            latest_commit: latest_commit.map(str::to_string),
            run: None,
            review_id: None,
            review_status: None,
            review_round: None,
            review_panel_id: None,
            review_panel_members: Vec::new(),
            review_panel_lease_state: None,
            review_feedback_outcome: None,
            review_feedback_threshold_reason: None,
            review_feedback_evaluated_at: None,
            review_feedback_blocking: Vec::new(),
            review_feedback_suggestions: Vec::new(),
            review_feedback_nitpicks: Vec::new(),
            review_feedback_dissent: Vec::new(),
            integration_conflict_owner: None,
            integration_conflict_source: None,
            integration_conflict_merge_target: None,
            integration_conflict_reviewed_commit: None,
            integration_conflict_previous_worker: None,
            integration_conflict_conflicting_files: Vec::new(),
            acceptance_criteria: Vec::new(),
            file_hints: Vec::new(),
            constraints: Vec::new(),
            test_requirements: Vec::new(),
            plan_steps: Vec::new(),
            implementation_notes: None,
            research_context: Vec::new(),
            proof: None,
            feedback: None,
        }
    }
    fn worker_set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn nudge_message_contains_task_id_commit_and_both_actions() {
        let msg = build_post_checkpoint_nudge_message("T-abc123", "deadbeefcafe", 137);
        assert!(msg.contains("T-abc123"));
        assert!(msg.contains("deadbeefcafe"));
        assert!(msg.contains("137s"));
        assert!(msg.contains("action=complete"));
        assert!(msg.contains("action=progress"));
        // Must explicitly remind that checkpoint does not transition status.
        assert!(msg.contains("does NOT transition"));
    }

    #[test]
    fn dirty_worktree_nudge_requires_lifecycle_tool_call() {
        let task = in_progress_task("T-dirty", "worker-a", None);
        let msg = build_dirty_worktree_handoff_nudge_message(
            &task,
            "mcp_brehon_task",
            "worktree has uncommitted changes",
            94,
        );
        assert!(msg.contains("Dirty worktree handoff required"));
        assert!(msg.contains("T-dirty"));
        assert!(msg.contains("94s"));
        assert!(msg.contains("action=complete"));
        assert!(msg.contains("action=checkpoint"));
        assert!(msg.contains("action=progress"));
        assert!(msg.contains("action=update"));
        assert!(msg.contains("Do not narrate completion"));
    }

    #[test]
    fn candidate_emitted_when_worker_idle_past_threshold_with_checkpointed_task() {
        let now = Instant::now();
        let idle_for = Duration::from_secs(120);
        let last_seen = now - idle_for;
        let tasks = [in_progress_task("T-1", "worker-a", Some("commit-a"))];
        let workers = worker_set(&["worker-a", "worker-b"]);
        let busy = HashSet::new();
        let mut last_activity = HashMap::new();
        last_activity.insert("worker-a".to_string(), last_seen);
        let nudges_sent = HashMap::new();

        let candidates = find_post_checkpoint_nudge_candidates(
            &tasks,
            &workers,
            &busy,
            &last_activity,
            &nudges_sent,
            Duration::from_secs(90),
            Duration::from_secs(600),
            now,
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].worker_id, "worker-a");
        assert_eq!(candidates[0].task_id, "T-1");
        assert_eq!(candidates[0].commit, "commit-a");
        assert_eq!(candidates[0].idle_secs, 120);
    }

    #[test]
    fn no_candidate_when_worker_still_within_idle_threshold() {
        let now = Instant::now();
        let tasks = [in_progress_task("T-1", "worker-a", Some("commit-a"))];
        let mut last_activity = HashMap::new();
        last_activity.insert("worker-a".to_string(), now - Duration::from_secs(30));

        let candidates = find_post_checkpoint_nudge_candidates(
            &tasks,
            &worker_set(&["worker-a"]),
            &HashSet::new(),
            &last_activity,
            &HashMap::new(),
            Duration::from_secs(90),
            Duration::from_secs(600),
            now,
        );
        assert!(candidates.is_empty());
    }

    #[test]
    fn no_candidate_when_task_has_no_latest_commit() {
        // Worker idle long enough but never checkpointed — that's the
        // ordinary stale-worker path, not a handoff gap. Existing
        // idle-recycle handles it.
        let now = Instant::now();
        let tasks = [in_progress_task("T-1", "worker-a", None)];
        let mut last_activity = HashMap::new();
        last_activity.insert("worker-a".to_string(), now - Duration::from_secs(600));

        let candidates = find_post_checkpoint_nudge_candidates(
            &tasks,
            &worker_set(&["worker-a"]),
            &HashSet::new(),
            &last_activity,
            &HashMap::new(),
            Duration::from_secs(90),
            Duration::from_secs(600),
            now,
        );
        assert!(candidates.is_empty());
    }

    #[test]
    fn no_candidate_when_worker_currently_executing_tool() {
        // Tool execution means the worker is actively doing something —
        // nudging would be noise. The idle clock also hasn't started yet.
        let now = Instant::now();
        let tasks = [in_progress_task("T-1", "worker-a", Some("commit-a"))];
        let mut last_activity = HashMap::new();
        last_activity.insert("worker-a".to_string(), now - Duration::from_secs(600));

        let candidates = find_post_checkpoint_nudge_candidates(
            &tasks,
            &worker_set(&["worker-a"]),
            &worker_set(&["worker-a"]), // busy!
            &last_activity,
            &HashMap::new(),
            Duration::from_secs(90),
            Duration::from_secs(600),
            now,
        );
        assert!(candidates.is_empty());
    }

    #[test]
    fn no_candidate_when_task_not_in_progress() {
        let now = Instant::now();
        let mut task = in_progress_task("T-1", "worker-a", Some("commit-a"));
        task.status = "review_ready".to_string();
        let tasks = [task];
        let mut last_activity = HashMap::new();
        last_activity.insert("worker-a".to_string(), now - Duration::from_secs(600));

        let candidates = find_post_checkpoint_nudge_candidates(
            &tasks,
            &worker_set(&["worker-a"]),
            &HashSet::new(),
            &last_activity,
            &HashMap::new(),
            Duration::from_secs(90),
            Duration::from_secs(600),
            now,
        );
        assert!(candidates.is_empty());
    }

    #[test]
    fn cooldown_prevents_duplicate_nudge_for_same_commit() {
        let now = Instant::now();
        let tasks = [in_progress_task("T-1", "worker-a", Some("commit-a"))];
        let mut last_activity = HashMap::new();
        last_activity.insert("worker-a".to_string(), now - Duration::from_secs(600));
        let mut nudges_sent = HashMap::new();
        nudges_sent.insert(
            (
                "worker-a".to_string(),
                "T-1".to_string(),
                "commit-a".to_string(),
            ),
            now - Duration::from_secs(60),
        );

        let candidates = find_post_checkpoint_nudge_candidates(
            &tasks,
            &worker_set(&["worker-a"]),
            &HashSet::new(),
            &last_activity,
            &nudges_sent,
            Duration::from_secs(90),
            Duration::from_secs(600),
            now,
        );
        assert!(candidates.is_empty());
    }

    #[test]
    fn new_checkpoint_commit_reopens_nudge_window() {
        // Dedup key includes the commit SHA. A fresh checkpoint commit
        // (different SHA) is treated as a new handoff opportunity even
        // though a nudge was recently sent for the previous commit.
        let now = Instant::now();
        let tasks = [in_progress_task("T-1", "worker-a", Some("commit-b"))];
        let mut last_activity = HashMap::new();
        last_activity.insert("worker-a".to_string(), now - Duration::from_secs(600));
        let mut nudges_sent = HashMap::new();
        nudges_sent.insert(
            (
                "worker-a".to_string(),
                "T-1".to_string(),
                "commit-a".to_string(),
            ),
            now - Duration::from_secs(60),
        );

        let candidates = find_post_checkpoint_nudge_candidates(
            &tasks,
            &worker_set(&["worker-a"]),
            &HashSet::new(),
            &last_activity,
            &nudges_sent,
            Duration::from_secs(90),
            Duration::from_secs(600),
            now,
        );
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].commit, "commit-b");
    }

    #[test]
    fn cooldown_expires_after_window() {
        let now = Instant::now();
        let tasks = [in_progress_task("T-1", "worker-a", Some("commit-a"))];
        let mut last_activity = HashMap::new();
        last_activity.insert("worker-a".to_string(), now - Duration::from_secs(1200));
        let mut nudges_sent = HashMap::new();
        // Sent 11 minutes ago — cooldown (10 min) has elapsed.
        nudges_sent.insert(
            (
                "worker-a".to_string(),
                "T-1".to_string(),
                "commit-a".to_string(),
            ),
            now - Duration::from_secs(11 * 60),
        );

        let candidates = find_post_checkpoint_nudge_candidates(
            &tasks,
            &worker_set(&["worker-a"]),
            &HashSet::new(),
            &last_activity,
            &nudges_sent,
            Duration::from_secs(90),
            Duration::from_secs(10 * 60),
            now,
        );
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn assignee_not_in_current_worker_set_is_ignored() {
        // Task references a worker who isn't in this session (e.g. was
        // reassigned). Don't attempt to nudge them.
        let now = Instant::now();
        let tasks = [in_progress_task("T-1", "ghost-worker", Some("commit-a"))];
        let mut last_activity = HashMap::new();
        last_activity.insert("ghost-worker".to_string(), now - Duration::from_secs(600));

        let candidates = find_post_checkpoint_nudge_candidates(
            &tasks,
            &worker_set(&["worker-a", "worker-b"]),
            &HashSet::new(),
            &last_activity,
            &HashMap::new(),
            Duration::from_secs(90),
            Duration::from_secs(600),
            now,
        );
        assert!(candidates.is_empty());
    }

    #[test]
    fn prune_drops_expired_and_reassigned_records() {
        let now = Instant::now();
        let mut nudges_sent: HashMap<(String, String, String), Instant> = HashMap::new();
        // Expired by cooldown.
        nudges_sent.insert(
            ("w".into(), "T-old".into(), "c-old".into()),
            now - Duration::from_secs(20 * 60),
        );
        // Task moved past in_progress — stale.
        nudges_sent.insert(
            ("w".into(), "T-moved".into(), "c-moved".into()),
            now - Duration::from_secs(60),
        );
        // Same task, different commit now — stale for THIS commit.
        nudges_sent.insert(
            ("w".into(), "T-rolled".into(), "c-old-rolled".into()),
            now - Duration::from_secs(60),
        );
        // Still valid.
        nudges_sent.insert(
            ("w".into(), "T-live".into(), "c-live".into()),
            now - Duration::from_secs(60),
        );

        let mut moved = in_progress_task("T-moved", "w", Some("c-moved"));
        moved.status = "review_ready".to_string();
        let tasks = vec![
            moved,
            in_progress_task("T-rolled", "w", Some("c-new-rolled")),
            in_progress_task("T-live", "w", Some("c-live")),
        ];

        prune_post_checkpoint_nudge_records(
            &mut nudges_sent,
            &tasks,
            Duration::from_secs(10 * 60),
            now,
        );

        assert!(!nudges_sent.contains_key(&("w".into(), "T-old".into(), "c-old".into())));
        assert!(!nudges_sent.contains_key(&("w".into(), "T-moved".into(), "c-moved".into())));
        assert!(!nudges_sent.contains_key(&("w".into(), "T-rolled".into(), "c-old-rolled".into())));
        assert!(nudges_sent.contains_key(&("w".into(), "T-live".into(), "c-live".into())));
    }

    #[test]
    fn live_reviewer_resend_requires_uncertain_unacknowledged_delivery() {
        let mut obligation = PendingReviewObligation {
            task_id: "T-review".to_string(),
            task_title: "Pending review".to_string(),
            review_id: "REV-review".to_string(),
            panel_id: Some("primary".to_string()),
            round: Some(1),
            pending_reviewers: 1,
            assignment_delivery_state: Some("drained_without_ack".to_string()),
            assignment_acknowledged_at: None,
        };
        assert!(live_reviewer_should_receive_resend(&obligation));

        obligation.assignment_delivery_state = Some("dead_lettered".to_string());
        assert!(live_reviewer_should_receive_resend(&obligation));

        obligation.assignment_delivery_state = Some("unknown".to_string());
        assert!(live_reviewer_should_receive_resend(&obligation));

        obligation.assignment_acknowledged_at = Some("2026-05-24T00:00:00Z".to_string());
        assert!(!live_reviewer_should_receive_resend(&obligation));

        obligation.assignment_acknowledged_at = None;
        obligation.assignment_delivery_state = Some("queued".to_string());
        assert!(!live_reviewer_should_receive_resend(&obligation));

        obligation.assignment_delivery_state = Some("injected".to_string());
        assert!(!live_reviewer_should_receive_resend(&obligation));
    }
}

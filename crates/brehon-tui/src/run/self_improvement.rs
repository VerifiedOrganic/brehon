//! Self-improvement and context-reset prompt builders.
//!
//! These functions construct startup prompts that are injected into agent
//! panes after session resets, recycles, or during idle self-improvement
//! cycles while a task waits on review.

use brehon_mux::{Mux, PaneKind};
use brehon_types::task::normalize_task_status;

use super::types::TaskInfo;

pub(crate) fn build_reviewer_reset_startup_prompt(mux: &Mux, reviewer: &str) -> Option<String> {
    let pane = mux.panes().find(|pane| pane.id() == reviewer)?;
    let caps = pane.cli_type().capabilities();
    let agent_cmd = format!("{}agent", caps.tool_prefix);
    let verification_cmd = format!("{}verification", caps.tool_prefix);
    Some(format!(
        "Brehon reviewer startup. You are reviewer '{reviewer}'.\n\
 1) Do NOT proactively discover, reconnect, or call Brehon MCP tools during idle startup. Stay idle until you receive a review request prompt or an explicit review-obligation nudge.\n\
 2) When real review work arrives, use the Brehon MCP verification tool directly from that turn. Do not preflight with {agent_cmd} action=session_start or {agent_cmd} action=whoami unless the prompt explicitly requires it.\n\
 3) Do not do supervisor planning work. Only the supervisor brainstorms, creates epics, or decomposes tasks.\n\
 4) Your checkout root is the current worktree directory. Treat every file path in review prompts, findings, and task titles as repository-relative to that root unless the prompt explicitly says otherwise.\n\
 5) When a review request arrives, do not edit files. Review the task-scoped diff and supplied context only.\n\
 6) Evaluate the code across these dimensions: correctness, security, performance, concurrency, error handling, maintainability.\n\
 7) Score 1-10: 1-3=reject, 4-5=major issues, 6=marginal, 7=acceptable, 8=good, 9-10=excellent.\n\
 8) Submit your review by calling the Brehon MCP tool `{verification_cmd}` directly. Do NOT use shell/Bash to run `{verification_cmd}` as a command string.\n\
 9) Required submit_review arguments: `action=submit_review`, `review_id=<review_id_from_prompt>`, `reviewer={reviewer}`, `score=<1-10>`, `verdict=<approved|needs_revision|rejected>`, and `findings=[{{\"description\":\"...\",\"file\":\"...\",\"line\":42,\"severity\":\"blocking|suggestion|nitpick\",\"suggestion\":\"...\"}}]`.\n\
 10) IMPORTANT: Use the review_id from the review request prompt, not the task_id. Always include reviewer={reviewer}.\n\
 11) Do not send a normal agent message instead of the structured review result.\n\
 12) After submitting the review, stop and wait for the next request.\n\
 13) Do not narrate MCP bootstrap/tool calls in normal text. Before any review request arrives, emit at most one short readiness line.\n\
 14) If Brehon MCP tools are temporarily unavailable when real review work arrives, stop and wait for a fresh turn instead of polling, sleeping, or running shell commands to check MCP availability."
    ))
}

pub(crate) fn build_advisor_reset_startup_prompt(mux: &Mux, advisor: &str) -> Option<String> {
    let pane = mux.panes().find(|pane| pane.id() == advisor)?;
    if pane.kind() != &PaneKind::Advisor {
        return None;
    }
    let caps = pane.cli_type().capabilities();
    let agent_cmd = format!("{}agent", caps.tool_prefix);
    let advisor_cmd = format!("{}advisor", caps.tool_prefix);
    Some(brehon_types::build_advisor_startup_prompt(
        advisor,
        &agent_cmd,
        &advisor_cmd,
        None,
    ))
}

pub(crate) fn build_research_reset_startup_prompt(mux: &Mux, researcher: &str) -> Option<String> {
    let pane = mux.panes().find(|pane| pane.id() == researcher)?;
    if pane.kind() != &PaneKind::Research {
        return None;
    }
    let caps = pane.cli_type().capabilities();
    let agent_cmd = format!("{}agent", caps.tool_prefix);
    let research_cmd = format!("{}research", caps.tool_prefix);
    Some(brehon_types::build_research_startup_prompt(
        researcher,
        &agent_cmd,
        &research_cmd,
        pane.configured_agent_type(),
        None,
    ))
}

pub(crate) fn build_worker_context_reset_startup_prompt(mux: &Mux, worker: &str) -> Option<String> {
    let pane = mux.panes().find(|pane| pane.id() == worker)?;
    if pane.kind() != &PaneKind::Worker {
        return None;
    }
    let caps = pane.cli_type().capabilities();
    let task_cmd = format!("{}task", caps.tool_prefix);
    let task_hint = pane
        .task_context()
        .map(|task| {
            format!(
                "2) You are still assigned task '{}' ({}). Resume from the current worktree state; do not restart from scratch. If you need to rehydrate the task details, call `{task_cmd} action=mine` at most once.\n",
                task.task_id, task.title
            )
        })
        .unwrap_or_else(|| {
            format!(
                "2) If you need to recover your assignment details after the reset, call `{task_cmd} action=mine` at most once, then continue from the current worktree state.\n"
            )
        });
    Some(format!(
        "Brehon worker session reset. Your previous model session was restarted after a recoverable provider/runtime failure.\n\
 1) Do NOT proactively call Brehon MCP bootstrap tools during reset recovery. Brehon already kept this worker pane registered.\n\
 {task_hint}\
 3) Stay on the current worker branch and keep the current worktree contents. Do NOT checkout `main` or discard local commits.\n\
 4) Continue the assigned task, checkpoint through Brehon before handoff, and report progress through `{task_cmd}` as normal.\n\
 5) If no task is assigned, send one ready message to {supervisor} and stop. Do not poll or loop on `{task_cmd} action=mine`.\n\
 6) Do not narrate MCP bootstrap/tool calls in normal text. Emit at most one short readiness line before resuming work.",
        supervisor = mux.supervisor_name()
    ))
}

pub(crate) fn build_worker_recycle_startup_prompt(mux: &Mux, worker: &str) -> Option<String> {
    let pane = mux.panes().find(|pane| pane.id() == worker)?;
    if pane.kind() != &PaneKind::Worker {
        return None;
    }
    let caps = pane.cli_type().capabilities();
    let task_cmd = format!("{}task", caps.tool_prefix);
    Some(format!(
        "Brehon worker recycle. Your previous task completed and this pane was reset to clear old worker context.\n\
 1) Do NOT proactively discover, reconnect, or call Brehon MCP tools during idle recycle startup. Stay idle until a real assignment prompt arrives.\n\
 2) When a new assignment arrives, call `{task_cmd} action=mine` at most once from that turn if you need the current task details.\n\
 3) Stay on the current dedicated worker branch/worktree. Do NOT checkout `main` or a task's `merge_target` branch in this pane.\n\
 4) Do not narrate MCP bootstrap/tool calls in normal text. Before a new assignment arrives, emit at most one short readiness line."
    ))
}

pub(crate) fn build_supervisor_reset_startup_prompt(
    mux: &Mux,
    supervisor: &str,
    host_owned: bool,
) -> Option<String> {
    let pane = mux.panes().find(|pane| pane.id() == supervisor)?;
    if pane.kind() != &PaneKind::Supervisor {
        return None;
    }
    let caps = pane.cli_type().capabilities();
    let agent_cmd = format!("{}agent", caps.tool_prefix);
    let task_cmd = format!("{}task", caps.tool_prefix);
    if host_owned {
        Some(format!(
            "Brehon supervisor session reset (unattended headless run). Your previous supervisor session was restarted after a runtime failure.\n\
 1) Your session is already registered. Do not call {agent_cmd} action=session_start or {agent_cmd} action=whoami.\n\
 2) Use {task_cmd} action=ready and {task_cmd} action=conflicts directly to inspect current state. If you need the full epic backlog context after the reset, call {task_cmd} action=list task_type=epic. Then act immediately. Do not wait for operator confirmation.\n\
 3) Resume supervisor-only orchestration. Do NOT implement ordinary worker tasks yourself.\n\
 4) If no action is required after checking state, emit one short status line and stop.\n\
 5) Do not narrate MCP bootstrap/tool calls in normal text."
        ))
    } else {
        Some(format!(
            "Brehon supervisor session reset. Your previous supervisor session was restarted after a runtime failure.\n\
 1) Call these silently, without narrating each step: {agent_cmd} action=session_start name={supervisor} agent_type=supervisor ; {agent_cmd} action=whoami\n\
 2) Rebuild live coordination context now: {task_cmd} action=list task_type=epic ; {task_cmd} action=conflicts ; {task_cmd} action=ready\n\
 3) Resume supervisor-only orchestration. Do NOT implement ordinary worker tasks yourself.\n\
 4) If no action is currently required after reloading context, emit one short status line and stop.\n\
 5) Do not narrate MCP bootstrap/tool calls in normal text."
        ))
    }
}

pub(crate) fn is_review_wait_task_status(status: &str) -> bool {
    matches!(
        normalize_task_status(status),
        Some("review_ready" | "in_review")
    )
}

pub(crate) fn self_improvement_task_is_mutating(task_name: &str) -> bool {
    matches!(
        task_name,
        "fix_warnings" | "update_documentation" | "refactor_duplicates"
    )
}

pub(crate) fn find_review_wait_task_for_worker<'a>(
    tasks: &'a [TaskInfo],
    worker: &str,
) -> Option<&'a TaskInfo> {
    let mut active: Option<&TaskInfo> = None;
    for task in tasks {
        if task.assignee.as_deref() != Some(worker) {
            continue;
        }
        if task.task_type != "task" || !is_review_wait_task_status(&task.status) {
            continue;
        }
        let replace = active
            .and_then(|current| {
                let current_updated = current.updated_at.as_deref().unwrap_or_default();
                let candidate_updated = task.updated_at.as_deref().unwrap_or_default();
                (candidate_updated > current_updated).then_some(true)
            })
            .unwrap_or(active.is_none());
        if replace {
            active = Some(task);
        }
    }
    active
}

pub(crate) fn build_task_scoped_self_improvement_prompt(
    task: &TaskInfo,
    task_name: &str,
    allow_mutating_idle_work: bool,
) -> Option<String> {
    let task_step = match task_name {
        "run_tests" => {
            "Run focused tests relevant to this task and the current task worktree only. Prefer targeted test commands over full-workspace sweeps when the scope is obvious."
        }
        "lint_check" | "run_lint" => {
            "Run focused lint, compile, or static checks relevant to the files and code paths for this task only. Prefer the narrowest useful command."
        }
        "collect_review_notes" | "analyze_feedback" => {
            "Inspect the current task diff, local worktree state, and any review-visible context for this same task. Summarize likely reviewer concerns or follow-up checks without editing files."
        }
        "fix_warnings" => {
            "Address warnings that are strictly within this same task's files and scope, then rerun the narrowest relevant validation."
        }
        "update_documentation" => {
            "Update documentation that belongs to this same task's implementation only, then verify it still matches the code."
        }
        "refactor_duplicates" => {
            "Refactor duplication only inside this same task's touched area if it is safe and directly relevant, then rerun the narrowest relevant validation."
        }
        _ => return None,
    };

    let mutating = self_improvement_task_is_mutating(task_name);
    if mutating && !allow_mutating_idle_work {
        return None;
    }

    let mutation_rule = if mutating {
        "Edits are allowed, but only inside this task's current worktree and only for the same task scope. Do not touch unrelated files, branches, or worktrees. If you make task-scoped edits, checkpoint through Brehon before stopping."
    } else {
        "Do not edit files, create commits, or change task status during this pass. Do not create build artifacts. Keep this self-improvement run non-mutating. For Go main packages, do not run bare `go build <package>` because it writes an executable into the current directory; use `go test`, `go build ./...`, or `go build -o /tmp/<name> <package>` instead."
    };

    Some(format!(
        "Brehon task-scoped self-improvement. You are still assigned task '{}' ({}), and it is currently `{}` while review is pending.\n\
 1) Stay in the current worktree and current worker branch for this same task. Do NOT inspect, checkout, or modify any other task, worktree, or branch.\n\
 2) Do NOT call `task action=mine`, ask for new work, or switch task context. This is background work only for the current task while it waits on review.\n\
 3) If any new task, review, or changes-requested prompt arrives, stop this self-improvement pass immediately and follow that newer prompt.\n\
 4) {mutation_rule}\n\
 5) Do this now: {task_step}\n\
 6) Keep the pass lightweight and task-scoped. Prefer targeted commands over whole-workspace sweeps unless there is no narrower option.\n\
 7) When finished, emit one short summary line and then wait.",
        task.id, task.title, task.status
    ))
}

pub(crate) fn next_self_improvement_prompt(
    task: &TaskInfo,
    task_names: &[String],
    allow_mutating_idle_work: bool,
    cursor: usize,
) -> Option<(usize, String, String)> {
    if task_names.is_empty() {
        return None;
    }
    for offset in 0..task_names.len() {
        let index = (cursor + offset) % task_names.len();
        let task_name = task_names[index].trim();
        if task_name.is_empty() {
            continue;
        }
        if let Some(prompt) =
            build_task_scoped_self_improvement_prompt(task, task_name, allow_mutating_idle_work)
        {
            return Some((index, task_name.to_string(), prompt));
        }
    }
    None
}

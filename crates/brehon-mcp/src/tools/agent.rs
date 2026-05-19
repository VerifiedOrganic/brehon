//! Agent lifecycle tool for MCP.
//!
//! Action-based tool for session registration, identity, and messaging.
//! The `session_start` action serves as Brehon's ACP handshake: agents call it
//! on startup and receive their full role-specific instructions via the MCP
//! response, eliminating the need for PTY text injection.
//!
//! The `message` action delivers messages to target agents via Claude Code's
//! native Teams inbox files at `~/.claude/teams/{team}/inboxes/{target}.json`.

use async_trait::async_trait;
use brehon_mux::{PromptQueueEntry, SessionScopedQueue};
use brehon_types::{
    build_advisor_startup_prompt, build_worker_protocol, infer_task_completion_mode,
    parse_task_completion_mode, WorkerBootstrapMode,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::McpError;
use crate::server::ToolResult;
use crate::tools::{error_result, text_result, Tool};

// ── Prompt-queue gateway ─────────────────────────────────────────────────────

/// Outcome of a prompt-delivery attempt from the MCP side.
///
/// `prompt_id` is always populated when `queued == true` and is the key
/// callers use to poll `agent action=delivery_status` for downstream delivery
/// state (`queued`, `injected`, `dead_lettered`, `drained_without_ack`). When
/// enqueue itself fails, `prompt_id` is empty and the caller has nothing to poll.
#[derive(Debug, Clone)]
pub struct DeliveryOutcome {
    pub queued: bool,
    pub method: String,
    pub prompt_id: String,
}

/// Deliver a message to a target agent via the prompt-queue gateway.
///
/// MCP tools cannot call `Mux::deliver_prompt()` directly (it lives in the
/// TUI process). The prompt-queue is the IPC bridge: MCP enqueues a
/// `PromptQueueEntry`, the TUI drains it and routes through
/// `Mux::deliver_prompt()` which handles Claude Code (Teams inbox) and
/// non-Claude (PTY injection) correctly.
///
/// This is the canonical delivery path for MCP → agent communication. The
/// returned [`DeliveryOutcome`] carries a prompt_id that the TUI will stamp
/// onto a delivery-ack file (`runtime/prompt-delivery-acks/<prompt_id>.json`)
/// once it successfully injects the prompt into the target pane.
pub fn try_deliver_message(target: &str, from: &str, message: &str) -> DeliveryOutcome {
    let Some(root) = brehon_root() else {
        return DeliveryOutcome {
            queued: false,
            method: "no BREHON_ROOT available".to_string(),
            prompt_id: String::new(),
        };
    };

    let session_name = resolved_runtime_session_name_for_prompt_queue(&root);
    let prompt_queue = SessionScopedQueue::new(&session_name, prompt_queue_root(&root));

    let entry = PromptQueueEntry::new(target, Some(from), message);
    let prompt_id = entry.prompt_id.clone().unwrap_or_default();
    match prompt_queue.enqueue(entry) {
        Ok(_) => {
            if let Err(err) =
                write_prompt_enqueue_ack(&root, &prompt_id, target, from, &session_name)
            {
                tracing::warn!(
                    prompt_id = %prompt_id,
                    target = %target,
                    error = %err,
                    "failed to write prompt enqueue ack"
                );
            }
            DeliveryOutcome {
                queued: true,
                method: "queued".to_string(),
                prompt_id,
            }
        }
        Err(e) => DeliveryOutcome {
            queued: false,
            method: format!("prompt-queue write failed: {e}"),
            prompt_id: String::new(),
        },
    }
}

pub(crate) const DEFAULT_SESSION_STALE_AFTER: Duration = Duration::from_secs(15 * 60);

fn session_stale_after() -> Duration {
    std::env::var("BREHON_SESSION_STALE_AFTER_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_SESSION_STALE_AFTER)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub(crate) fn current_runtime_session_name_from_root(root: &std::path::Path) -> Option<String> {
    let path = root.join("runtime").join("current-session.json");
    let content = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&content).ok()?;
    value
        .get("session_name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub(crate) fn current_runtime_session_name() -> Option<String> {
    brehon_root().and_then(|root| current_runtime_session_name_from_root(&root))
}

pub(crate) fn session_matches_current_runtime(entry: &Value) -> bool {
    let Some(expected) = current_runtime_session_name() else {
        return true;
    };
    entry
        .get("session_name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        == Some(expected.as_str())
}

fn session_path(agent_name: &str) -> Option<PathBuf> {
    brehon_root().map(|root| {
        root.join("runtime")
            .join("sessions")
            .join(format!("{agent_name}.json"))
    })
}

fn project_root_from_brehon_root() -> Option<PathBuf> {
    let brehon_root = std::env::var("BREHON_ROOT").ok().map(PathBuf::from)?;
    brehon_root.parent().map(|path| path.to_path_buf())
}

fn load_project_config() -> Option<brehon_types::BrehonConfig> {
    let project_root = project_root_from_brehon_root()?;
    brehon_config::load_config(Some(&project_root)).ok()
}

fn configured_reviewer_prompt(
    config: &brehon_types::BrehonConfig,
    agent_type: Option<&str>,
) -> Option<String> {
    let agent_type = agent_type?.trim();
    if agent_type.is_empty() {
        return None;
    }

    config
        .roles
        .reviewers
        .iter()
        .find(|reviewer| reviewer.lane == agent_type)
        .and_then(|reviewer| {
            config
                .lane_system_prompt(agent_type, reviewer.system_prompt.as_deref())
                .map(str::to_string)
        })
}

fn configured_advisor_prompt(
    config: &brehon_types::BrehonConfig,
    agent_type: Option<&str>,
) -> Option<String> {
    let agent_type = agent_type?.trim();
    if agent_type.is_empty() {
        return None;
    }

    config
        .advisors
        .pools
        .iter()
        .find(|pool| pool.lane == agent_type)
        .and_then(|pool| {
            config
                .lane_system_prompt(agent_type, pool.system_prompt.as_deref())
                .map(str::to_string)
        })
}

fn configured_research_prompt(
    config: &brehon_types::BrehonConfig,
    agent_type: Option<&str>,
) -> Option<String> {
    let agent_type = agent_type?.trim();
    if agent_type.is_empty() {
        return None;
    }

    config
        .research
        .pools
        .iter()
        .find(|pool| pool.lane == agent_type || pool.id == agent_type)
        .and_then(|pool| {
            pool.instruction_profile.clone().or_else(|| {
                config
                    .lane_system_prompt(agent_type, None)
                    .map(str::to_string)
            })
        })
}

fn append_policy_sections(
    mut instructions: String,
    configured_prompt: Option<&str>,
    project_policy: Option<&str>,
) -> String {
    if let Some(prompt) = configured_prompt
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        instructions.push_str("\n\nAdditional lane policy:\n");
        instructions.push_str(prompt);
    }
    if let Some(policy) = project_policy
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        instructions.push_str("\n\nProject policy:\n");
        instructions.push_str(policy);
    }
    instructions
}

pub(crate) fn session_is_live(entry: &Value) -> bool {
    let timestamp = entry
        .get("last_seen_at")
        .and_then(|v| v.as_str())
        .or_else(|| entry.get("registered_at").and_then(|v| v.as_str()))
        .or_else(|| entry.get("started_at").and_then(|v| v.as_str()));

    let Some(timestamp) = timestamp else {
        return true;
    };

    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(timestamp) else {
        return true;
    };

    chrono::Utc::now().signed_duration_since(parsed.with_timezone(&chrono::Utc))
        <= chrono::Duration::from_std(session_stale_after())
            .unwrap_or_else(|_| chrono::Duration::seconds(15 * 60))
}

pub(crate) fn refresh_current_session_file() {
    let agent_name = match std::env::var("BREHON_AGENT_NAME") {
        Ok(value) if !value.is_empty() => value,
        _ => return,
    };
    let role = match std::env::var("BREHON_AGENT_ROLE") {
        Ok(value) if !value.is_empty() => value,
        _ => return,
    };
    let session_id = match std::env::var("BREHON_SESSION_ID") {
        Ok(value) if !value.is_empty() => value,
        _ => return,
    };
    let agent_type = std::env::var("BREHON_AGENT_TYPE").ok();
    let model = std::env::var("BREHON_AGENT_MODEL").ok();
    let reasoning_effort = std::env::var("BREHON_REASONING_EFFORT").ok();

    write_session_file_with_metadata(
        &agent_name,
        &role,
        &session_id,
        agent_type.as_deref(),
        model.as_deref(),
        reasoning_effort.as_deref(),
    );
}

fn session_timestamp(entry: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let timestamp = entry
        .get("last_seen_at")
        .and_then(|v| v.as_str())
        .or_else(|| entry.get("registered_at").and_then(|v| v.as_str()))
        .or_else(|| entry.get("started_at").and_then(|v| v.as_str()))?;

    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|parsed| parsed.with_timezone(&chrono::Utc))
}

fn discover_live_supervisor_name() -> Option<String> {
    let root = brehon_root()?;
    let sessions_dir = root.join("runtime").join("sessions");
    let entries = std::fs::read_dir(sessions_dir).ok()?;

    let mut supervisors = Vec::new();
    for entry in entries.flatten() {
        let content = match std::fs::read_to_string(entry.path()) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let value: Value = match serde_json::from_str(&content) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if value.get("role").and_then(|v| v.as_str()) != Some("supervisor") {
            continue;
        }
        if !session_is_live(&value) {
            continue;
        }
        if !session_matches_current_runtime(&value) {
            continue;
        }
        let Some(name) = value.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        supervisors.push((session_timestamp(&value), name.to_string()));
    }

    supervisors.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    supervisors.into_iter().next().map(|(_, name)| name)
}

pub(crate) fn resolve_supervisor_name(explicit: Option<&str>) -> Option<String> {
    explicit
        .filter(|name| !name.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("BREHON_SUPERVISOR_NAME")
                .ok()
                .filter(|name| !name.trim().is_empty())
        })
        .or_else(discover_live_supervisor_name)
}

pub(crate) fn prompt_queue_root(root: &Path) -> PathBuf {
    root.join("runtime").join("prompt-queue")
}

fn prompt_enqueue_ack_dir(root: &Path) -> PathBuf {
    root.join("runtime").join("prompt-enqueue-acks")
}

fn write_prompt_enqueue_ack(
    root: &Path,
    prompt_id: &str,
    target: &str,
    from: &str,
    session_name: &str,
) -> Result<(), String> {
    if prompt_id.trim().is_empty() {
        return Ok(());
    }
    let dir = prompt_enqueue_ack_dir(root);
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("Failed to create prompt enqueue ack dir: {err}"))?;
    let payload = serde_json::json!({
        "prompt_id": prompt_id,
        "target": target,
        "from": from,
        "session_name": session_name,
        "queued_at": chrono::Utc::now().to_rfc3339(),
    });
    let path = dir.join(format!("{}.json", sanitize_prompt_id_for_path(prompt_id)));
    let data = serde_json::to_string_pretty(&payload)
        .map_err(|err| format!("Failed to serialize prompt enqueue ack: {err}"))?;
    std::fs::write(path, data).map_err(|err| format!("Failed to write prompt enqueue ack: {err}"))
}

pub(crate) fn resolve_session_name_for_write(root: &Path) -> Option<String> {
    if let Some(name) = std::env::var("BREHON_SESSION_NAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Some(name);
    }
    current_runtime_session_name_from_root(root)
}

fn resolved_runtime_session_name_for_prompt_queue(root: &std::path::Path) -> String {
    resolve_session_name_for_write(root).unwrap_or_else(|| {
        tracing::error!(
            brehon_root = %root.display(),
            "runtime/current-session.json missing or unreadable; prompt queue writes \
             will be stamped with __missing-runtime-session__ and swept by live sessions"
        );
        "__missing-runtime-session__".to_string()
    })
}

/// Build worker instructions returned by `session_start`.
fn worker_instructions(name: &str, supervisor: &str, project_policy: Option<&str>) -> String {
    let protocol = build_worker_protocol(
        WorkerBootstrapMode::SessionRegistered,
        "agent",
        "task",
        supervisor,
    );
    append_policy_sections(
        format!(
            "You are worker '{name}'. Your supervisor is '{supervisor}'.\n\nProtocol:\n{protocol}"
        ),
        None,
        project_policy,
    )
}

/// Build supervisor instructions returned by `session_start`.
fn supervisor_instructions(
    name: &str,
    configured_prompt: Option<&str>,
    project_policy: Option<&str>,
) -> String {
    append_policy_sections(
        format!(
            "You are factory supervisor '{name}' coordinating workers.\n\
        \n\
        Hard Rules:\n\
        - Use Brehon MCP tools only for coordination. For inter-agent messaging, use \
          `agent action=message target=<name> message=\"...\"` — NEVER use SendMessage.\n\
        - Do NOT invent shorthand tool actions. Use the exact tool action names shown by the tool contract \
          (for example `assign_workers`, not `assign`; `spawn_workers`, not `spawn`).\n\
        - NEVER implement worker tasks yourself. You are a coordinator, not a coder.\n\
        - NEVER edit files, write code, run tests, or do any implementation work.\n\
        - NEVER tell workers to merge or close tasks. Workers CANNOT merge. \
          YOU are the ONLY one who can close/merge tasks.\n\
        - NEVER treat a verbal \"done\" as completion. Completion requires passing review.\n\
        - NEVER poll aimlessly after assigning work. End your turn and wait.\n\
        - NEVER create standalone work items. Organize work as either:\n\
          * one epic with subtasks for a bounded feature, or\n\
          * one initiative containing multiple epics and their subtasks for a multi-phase plan.\n\
        - Do not paste full MCP/task output. Read silently; send concise decisions only.\n\
        \n\
        Startup Protocol:\n\
        1. Call agent action=whoami to confirm identity.\n\
        2. Call task action=list to see existing initiatives, epics, and in-progress work.\n\
        2a. Call search_skills query=\"\" to load the current brehon-* supervisor skill set, then \
            call search_rules query=\"\" to refresh project conventions before planning \
            or dispatch. Use brehon-discovery for request/design, brehon-breakdown for hierarchy \
            creation, brehon-dispatch for execution, and brehon-supervisor-checklist for recovery/closeout.\n\
        3. Call task action=list task_type=initiative to see active initiatives.\n\
        4. Call task action=list task_type=epic to see all epics and their subtask progress.\n\
        4b. Call task action=conflicts to see any supervisor-owned integration conflicts. \
            These are highest-priority unblockers. Resolve or explicitly triage them before \
            normal worker dispatch.\n\
        5. Call factory action=worker_status to see registered workers and whether they are \
           idle or busy with existing non-terminal tasks. `worker_status` also returns each \
           worker's `agent_type` (lane), `model`, `reasoning_effort`, `assignment_mode`, and \
           `accepted_work_classes`; use that metadata to \
           route simple/local work to cheaper or faster lanes and route risky, ambiguous, \
           cross-cutting, high-blast-radius, or repeatedly failing tasks to stronger lanes. \
           Reserved lanes such as codex-hardening are only for tasks whose execution_policy \
           targets that lane and accepted work class.\n\
        6. If any task has an assignee not matching a registered worker, reassign it \
           using factory action=assign_workers.\n\
        7. Call task action=ready to see the current frontier. It returns both:\n\
            - `integration_conflict_tasks`: supervisor-owned integration conflicts that must be resolved or explicitly triaged before any other queue\n\
            - `tasks`: unassigned pending worker tasks\n\
            - `review_ready_tasks`: tasks awaiting formal `request_review`\n\
            - `changes_requested_tasks`: unassigned revision tasks that need worker reassignment\n\
            - `stalled_tasks`: ASSIGNED changes_requested tasks whose worker has been silent past the stall threshold (the worker may have hallucinated a handoff or missed the nudge)\n\
            - `approved_tasks`: approved merge-flow tasks awaiting supervisor `task action=integrate`\n\
            - `followup_source_tasks`: tasks with open approved-review followups that should usually be promoted into real cleanup tasks\n\
            If `integration_conflict_tasks` is non-empty, resolve or explicitly triage those before requesting review, integrating approved work, or dispatching new worker tasks.\n\
            If `review_ready_tasks` is non-empty, request review for those before treating the frontier as empty.\n\
            If `changes_requested_tasks` is non-empty, reassign those revision tasks to idle workers before pulling new pending work.\n\
            If `stalled_tasks` is non-empty, investigate before re-nudging — call `agent action=delivery_status prompt_id=<id>` with the prompt_id of your last message to that worker to see whether it was injected or dead-lettered, and `factory action=worker_status` to see the worker's `nudge.nudge_delivery_state` (`Delivered` → `Acknowledged` → `ActedOn` or `TimedOut`). If the nudge never acknowledged, the worker never saw it; if acknowledged but not acted on, the worker saw it and ignored it — reassign rather than re-nudging.\n\
            If `approved_tasks` is non-empty, integrate those before declaring the frontier blocked.\n\
            If `followup_source_tasks` is non-empty, inspect them with `task action=followups id=<task-id>` and default to `task action=promote_followups id=<task-id>`; use `waive_followups` only for explicit no-action-needed items with specific IDs and reasons.\n\
        7a. You may also call task action=list status=review_ready and task action=list status=changes_requested to inspect those queues directly.\n\
        7b. For any tasks with status 'in_review', check if the review is stuck:\n\
            verification action=review_status task_id=<id>\n\
            If the panel has dead reviewers (not in current session), reassign:\n\
            verification action=reassign_panel task_id=<id>\n\
            The review council is frozen per round. Reassignment replaces dead reviewers \
            with new eligible reviewers and re-sends prompts; it does NOT silently shrink \
            the council.\n\
        7c. After any later action that may change the frontier (`close`, `integrate`, \
            reassignment, unblock, followup promotion/waiver, or any dependency-clearing transition), call \
            task action=ready again before ending your turn. If `integration_conflict_tasks` appear, \
            resolve or explicitly triage them before anything else. If idle workers exist and \
            `tasks` or unassigned `changes_requested_tasks` appear, dispatch them immediately \
            instead of leaving work queued. Do not leave `followup_source_tasks` sitting \
            unresolved once you know they exist.\n\
        \n\
        Hierarchy Workflow (MANDATORY — this is how ALL work is organized):\n\
        8. When the user requests work, decide the top-level container:\n\
           - If the request is one bounded feature, create one epic.\n\
           - If the request is a multi-phase plan or roadmap, create one initiative first, \
             then create one epic per phase under that initiative.\n\
        9. For a multi-phase initiative, create the initiative first:\n\
           task action=create task_type=initiative title=\"Initiative Name\" \
           description=\"Program-level problem statement and scope\" \
           acceptance_criteria=[\"Phase completion outcome 1\", \"Phase completion outcome 2\"] \
           plan_steps=[\"Phase 1\", \"Phase 2\", \"Phase 3\"] \
           implementation_notes=\"Sequencing, risks, and coordination notes\"\n\
        10. Then create each phase epic under the initiative:\n\
           task action=create task_type=epic parent_id=<initiative_id> title=\"Phase 1\" \
           description=\"One phase with a coherent deliverable\" \
           acceptance_criteria=[\"Observable phase outcome\"] \
           plan_steps=[\"Workstream A\", \"Workstream B\"] \
           implementation_notes=\"Phase-specific architecture notes\"\n\
           After phase epics exist, run: task action=ensure_final_hardening id=<initiative_id>. \
           This creates or backfills the single final cleanup debt surface and its seeded strong-model hardening tasks.\n\
        11. For a bounded request, create one epic directly:\n\
           task action=create task_type=epic title=\"Feature Name\" \
           description=\"High-level problem statement and goal\" \
           acceptance_criteria=[\"Observable outcome 1\", \"Observable outcome 2\"] \
           plan_steps=[\"Phase 1\", \"Phase 2\"] \
           implementation_notes=\"Key constraints, risks, or architecture notes\"\n\
           IMPORTANT: pass acceptance_criteria, plan_steps, file_hints, test_requirements, and \
           implementation_notes as TOP-LEVEL task args whenever possible. Do not bury them only \
           inside freeform description prose.\n\
        12. Then decompose each epic into subtasks, each linked to the epic:\n\
           task action=create title=\"Subtask 1\" parent_id=<epic_id> \
           description=\"One clear outcome and why it matters\" \
           acceptance_criteria=[\"What must be true when done\"] \
           file_hints=[\"crates/...\", \"docs/...\"] \
           test_requirements=[\"cargo test -p ...\"] \
           plan_steps=[\"Step 1\", \"Step 2\"] \
           implementation_notes=\"Important design constraints or gotchas\"\n\
           Repeat for each subtask. Each subtask should:\n\
           - Have ONE clear outcome\n\
           - Include concrete acceptance criteria\n\
           - Include file or area hints when known\n\
           - Include required tests or verification steps\n\
           - Include a short execution plan or implementation notes\n\
           - Be small enough for a single worker\n\
           - Use `completion_mode=close` for audit, research, design, docs, or other no-code tasks\n\
           - NEVER target live `.brehon` control-plane state (`.brehon/config*`, `.brehon/runtime/*`, `.brehon/worktrees/*`) in a worker task. \
             Handle Brehon self-repair and live orchestration maintenance directly as supervisor work instead.\n\
           - If a model insists on putting structure into description, use explicit sections \
             like `Acceptance Criteria:`, `File Hints:`, `Test Requirements:`, `Plan:`, and \
             `Implementation Notes:` so the task tool can recover it canonically\n\
           Thin merge-mode tasks will be rejected by the task tool.\n\
        13. Assign worker tasks only (NOT initiatives or epics):\n\
           factory action=assign_workers task_id=<task_id> workers=<worker_name>\n\
           IMPORTANT: workers are single-task while using per-worker worktrees. Do NOT assign \
           a new task to a worker until its current task reaches a terminal state (`merged` or \
           `closed`). Review-held and approved tasks still reserve the worker/worktree.\n\
        14. Use task action=children id=<initiative_or_epic_id> to check hierarchy progress.\n\
        15. When ALL subtasks of an epic are merged/approved, close the epic:\n\
            task action=close id=<epic_id>\n\
            Subtask lifecycle: in_progress → in_review → approved → merged (completion_mode=merge)\n\
            or in_progress → in_review → approved → closed (completion_mode=close).\n\
            Epic lifecycle: open → closed (when all subtasks merged).\n\
        16. When ALL epics under an initiative are closed, close the initiative:\n\
            task action=close id=<initiative_id>\n\
        \n\
        Review Protocol (MANDATORY — never skip):\n\
        17. When a worker reports a task as complete, initiate formal review:\n\
            verification action=request_review task_id=<subtask_id>\n\
            Do not copy a commit hash from chat history. For merge-mode tasks, \
            request_review uses the task's recorded latest_commit as the source of truth. \
            Pass commit=<hash> only if the task has no latest_commit; Brehon rejects \
            commit values that do not match the recorded checkpoint.\n\
            Treat chat history, prior reviewer findings, and worker prose as stale context unless \
            the current task/review tool response confirms the same fact. Prefer the structured \
            `next_action` returned by task/review tools over inferred workflow steps.\n\
            This freezes a council of all eligible reviewers currently available for that round. \
            Every council member must review unless the system performs an explicit reassignment \
            or timeout escalation. Audit-only or no-code-change tasks \
            still begin with request_review; the review gate is not optional. \
            NEVER use task action=update status=in_review; that bypasses review runtime state and \
            leaves reviewers idle.\n\
        18. Wait for the consolidated review report (delivered automatically).\n\
        19. If APPROVED: YOU close the subtask yourself:\n\
            Read the report first. It will tell you the task completion mode and merge target.\n\
            - If completion_mode=close: run `task action=close id=<subtask_id>`.\n\
            - If completion_mode=merge and merge_target is the default branch: do NOT run raw \
              `git commit`, `git merge`, or `git cherry-pick` on that protected branch. Prefer the \
              hierarchy flow where only `task action=close id=<top-level-container>` performs the \
              controlled final merge. Direct-to-default task exceptions require deliberate human repair.\n\
            - If completion_mode=merge and merge_target is an epic branch: YOU integrate the \
              reviewed commit into the epic branch, then run `task action=integrate id=<subtask_id>`.\n\
            Audit/no-code tasks should end in 'closed', not 'merged'. Only the final epic close \
            may merge to main. Brehon installs a Git hook that blocks agent-created commits on the \
            default branch; do not bypass it with BREHON_ALLOW_PROTECTED_BRANCH_COMMIT.\n\
            NEVER tell the worker to close/merge or perform post-approval integration — they cannot do it.\n\
        20. If CHANGES_REQUESTED: route the task back to a worker with \
            factory action=assign_workers task_id=<subtask_id> workers=<worker_name>, \
            then message that worker with the blocking feedback from the \
            consolidated report. The task will also carry structured `review_feedback` \
            for the assignee. Worker fixes and resubmits. Then call \
            verification action=request_review again for the next round.\n\
        21. If round reaches max (3): do not approve by override. Reset rounds, reseat/reassign \
            reviewers, or mark changes requested/rejected. Approval requires reviewer verdicts \
            satisfying policy.\n\
        22. NEVER manually message individual reviewers. Use request_review.\n\
        23. To check review progress: verification action=review_status task_id=<id>\n\
        \n\
        Dispatch Protocol:\n\
        24. After assigning work, END YOUR TURN. Do not poll.\n\
        25. When a worker or review report messages you, process it and decide next action.\n\
        25a. If task action=ready reports `integration_conflict_tasks`, or task action=conflicts \
             returns any items, prioritize them before assigning new feature work. \
             Supervisor-owned integration conflicts are not ordinary worker tasks.\n\
        25b. Integration state-machine recovery. `task action=integrate` is driven by an explicit \
             state machine (phases: null → cherry_picking → resolved → complete, plus aborted). \
             If a call returns phase=cherry_picking with conflicting_files, resolve them in the \
             integration worktree (either by hand or by letting the worker rebase/amend) and call \
             `task action=integrate id=<id>` again — the same call resumes from git state. \
             If the worktree is stuck (stale CHERRY_PICK_HEAD for an unrelated commit, or a \
             supervisor decided to back out), exit cleanly with \
             `task action=abort-integration id=<id> reason=\"...\"`; this restores status=approved \
             and marks phase=aborted. To retry after abort, or to recover from an irrecoverable \
             phase with known-good git state, use `task action=integrate id=<id> force=true` — \
             the force flag discards prior integration state and starts fresh, logs the prior \
             phase to tracing, and still refuses to re-run an already-completed integration \
             (which requires a manual revert first).\n\
        25c. Always inspect the `warnings` array on `task action=integrate` and `task action=close` \
             responses. A non-empty `warnings` array means the task closed successfully but an \
             auxiliary step failed; each entry carries `kind`, `message`, and `supervisor_action`. \
             The most common case is `worker_recycle_enqueue_failed`, which means the worker's \
             agent pane will retain stale task context until you recycle it manually (via the \
             brehon-tui Recycle keybind, or kill+respawn). Do not assign new work to that worker \
             until the recycle has been performed.\n\
        26. If all subtasks of an epic are merged, close the epic.\n\
        27. If all epics of an initiative are closed, close the initiative.\n\
        28. If the user gives new work, decide whether it belongs in an existing initiative, \
            a new epic, or a new initiative.\n\
        29. After completing the startup protocol, if there are no tasks or epics, \
            simply state you are ready and wait for the user prompt. \
            The user's initial message contains the work request. \
            Do NOT try to message a 'director' or send status reports — \
            just proceed to create the correct hierarchy from the user's instructions."
        ),
        configured_prompt,
        project_policy,
    )
}

fn task_completion_mode_for_context(task: &serde_json::Map<String, Value>) -> &'static str {
    let task_type = task
        .get("task_type")
        .and_then(|v| v.as_str())
        .unwrap_or("task");
    if matches!(task_type, "epic" | "initiative") {
        return "close";
    }

    let title = task.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let description = task
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    task.get("completion_mode")
        .and_then(|v| v.as_str())
        .and_then(parse_task_completion_mode)
        .unwrap_or_else(|| infer_task_completion_mode(title, description))
        .as_str()
}

/// Build reviewer instructions returned by `session_start`.
fn reviewer_instructions(
    name: &str,
    configured_prompt: Option<&str>,
    project_policy: Option<&str>,
    lease_mode: brehon_types::config::ReviewLeaseMode,
) -> String {
    let lease_policy = match lease_mode {
        brehon_types::config::ReviewLeaseMode::Exclusive => {
            "2c. Reviews are assigned through leased panels, not ad-hoc reviewer reuse. \
            If your panel is leased to a task, that same task keeps the panel across \
            `changes_requested` rereview cycles until the task is terminal or the panel \
            is explicitly released.\n\\
        2d. Do NOT assume you are idle just because another review is active or `tasks` \
            is empty. If `review_obligations` is non-empty, you still owe that review.\n\\"
        }
        brehon_types::config::ReviewLeaseMode::ShareAfterSubmit => {
            "2c. Reviews are assigned through shared-after-submit panels. You stay reserved for the \
            current task until Brehon confirms your structured submission and hard-resets your reviewer \
            session. Only after that reset may Brehon reuse you for another task.\n\\
        2d. Do NOT assume you are idle just because another review is active or `tasks` is empty. \
            If `review_obligations` is non-empty, you still owe that review. After submit_review succeeds, \
            stop and wait for either a fresh startup prompt or a new review request.\n\\"
        }
    };
    append_policy_sections(
        format!(
            "You are reviewer '{name}'.\n\
        \n\
        Protocol:\n\
        1. Call the agent tool with action=whoami to confirm identity.\n\
        2. On startup, call `task action=mine` at most once if you need to confirm \
            whether you already owe a review.\n\
        2a. Reviewer obligations appear in the `review_obligations` array even when `tasks` is empty. \
            If you have no active obligation after that one check, END THE TURN. \
            Do not keep a long-running idle turn open waiting for work. \
            New review requests arrive as separate prompts.\n\
        2b. If you need to confirm what you still owe later, call `task action=mine`. \
            Reviewer obligations appear in the `review_obligations` array even when `tasks` is empty.\n\
        {lease_policy}\
        3. Do NOT do supervisor planning work. Do not brainstorm, create epics, or decompose tasks.\n\
        4. When a review request arrives, do not edit files. Review the task-scoped diff and \
           supplied context only.\n\
        5. Evaluate across 6 dimensions: correctness, security, performance, concurrency, \
           error handling, and maintainability.\n\
        6. Assign a score from 1-10:\n\
           1-3 = Reject (fundamental issues)\n\
           4-5 = Blocking changes required\n\
           6-7 = Non-blocking issues or conditional approval\n\
           8-9 = Approve with minor suggestions\n\
           10  = Strong approve\n\
        7. List specific findings with file/line references where possible.\n\
        8. Submit your review (IMPORTANT: include reviewer=<your_name>):\n\
           verification action=submit_review review_id=<review_id_from_prompt> \
           reviewer={name} score=<1-10> verdict=<approved|needs_revision|rejected> \
           summary=\"Your review summary\" \
           findings='[{{\"description\":\"...\", \"file\":\"...\", \"line\":42, \
           \"severity\":\"blocking|suggestion|nitpick\", \"suggestion\":\"optional fix\"}}]'\n\
        9. Do NOT send a free-form agent message instead of the structured review.\n\
        10. NEVER call verification actions that manage panels or review gates: request_review, \
            reseat_panel, reassign_panel, release_panel, reset_rounds, or override. Those are \
            supervisor/maintenance actions. If panel seating is broken, stop after reporting your \
            review status; the supervisor must repair it.\n\
        11. After submitting, stop and wait. Your obligation is complete only when the \
            active round no longer appears in `review_obligations` or the panel is \
            explicitly released by the supervisor."
        ),
        configured_prompt,
        project_policy,
    )
}

/// Build advisor instructions returned by `session_start`.
fn advisor_instructions(
    name: &str,
    configured_prompt: Option<&str>,
    project_policy: Option<&str>,
) -> String {
    append_policy_sections(
        build_advisor_startup_prompt(name, "agent", "advisor", None),
        configured_prompt,
        project_policy,
    )
}

/// Build research instructions returned by `session_start`.
fn research_instructions(
    name: &str,
    configured_prompt: Option<&str>,
    project_policy: Option<&str>,
) -> String {
    append_policy_sections(
        format!(
            "You are research agent '{name}'.\n\
        \n\
        Protocol:\n\
        1. Call agent action=whoami to confirm identity.\n\
        2. Claim work with `research action=claim_next`. If your pool is known, pass `pool=<pool-id>`.\n\
        3. Research jobs are read-only. Do not edit repository files, task status, review state, or runtime control-plane files except through `research action=submit`.\n\
        4. Produce structured, cited context. Prefer concise summaries, concrete file/spec references, and uncertainty notes over broad prose.\n\
        5. Submit completed work with `research action=submit task_id=<task_id> job_id=<job_id> summary=\"...\" content=\"...\" citations='[...]'`.\n\
        6. If a job cannot be answered from available context, submit a brief artifact explaining the gap instead of blocking a worker.\n\
        7. After submitting or finding no queued job, stop and wait. Do not poll in a tight loop."
        ),
        configured_prompt,
        project_policy,
    )
}

// ── State recovery: build context summary from persisted files ──────────────

fn brehon_root() -> Option<PathBuf> {
    std::env::var("BREHON_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(".brehon")))
}

/// Build a state summary for the supervisor at session_start.
/// Reads all task and session files and produces a human-readable context
/// block that gets injected into the session_start response — just like
/// old_brehon's `build_context_with_stores()` did from SQLite.
fn build_supervisor_context() -> String {
    let Some(root) = brehon_root() else {
        return String::new();
    };

    let mut ctx = String::from("\n\n== CURRENT STATE (recovered from previous sessions) ==\n");

    // ── Tasks ──
    let tasks_dir = root.join("runtime").join("tasks");
    let mut tasks: Vec<serde_json::Map<String, Value>> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&tasks_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(t) = serde_json::from_str::<serde_json::Map<String, Value>>(&content) {
                    tasks.push(t);
                }
            }
        }
    }

    if tasks.is_empty() {
        ctx.push_str("\nTasks: None — fresh workspace, no previous work.\n");
    } else {
        ctx.push_str(&format!("\nTasks ({}):\n", tasks.len()));
        for task in &tasks {
            let id = task.get("task_id").and_then(|v| v.as_str()).unwrap_or("?");
            let title = task.get("title").and_then(|v| v.as_str()).unwrap_or("?");
            let status = task.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            let assignee = task
                .get("assignee")
                .and_then(|v| v.as_str())
                .unwrap_or("unassigned");
            let task_type = task
                .get("task_type")
                .and_then(|v| v.as_str())
                .unwrap_or("task");
            let completion_mode = task_completion_mode_for_context(task);
            ctx.push_str(&format!(
                "  - {id} [{task_type}] \"{title}\" status={status} assignee={assignee} completion_mode={completion_mode}\n"
            ));
        }
    }

    // ── Registered workers ──
    let sessions_dir = root.join("runtime").join("sessions");
    let mut workers: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(v) = serde_json::from_str::<Value>(&content) {
                    let role = v["role"].as_str().unwrap_or("");
                    let name = v["name"].as_str().unwrap_or("");
                    if role == "worker" && !name.is_empty() {
                        if !session_matches_current_runtime(&v) {
                            continue;
                        }
                        workers.push(name.to_string());
                    }
                }
            }
        }
    }

    // Also collect reviewers
    let mut reviewers: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(v) = serde_json::from_str::<Value>(&content) {
                    let role = v["role"].as_str().unwrap_or("");
                    let name = v["name"].as_str().unwrap_or("");
                    if role == "reviewer" && !name.is_empty() {
                        if !session_matches_current_runtime(&v) {
                            continue;
                        }
                        reviewers.push(name.to_string());
                    }
                }
            }
        }
    }

    if workers.is_empty() {
        ctx.push_str(
            "\nRegistered Workers: none yet (workers may still be starting up).\n\
             Workers will send a ready message once they register.\n",
        );
    } else {
        ctx.push_str(&format!("\nRegistered Workers ({}):\n", workers.len()));
        for w in &workers {
            ctx.push_str(&format!("  - {w}\n"));
        }
    }

    if !reviewers.is_empty() {
        ctx.push_str(&format!("\nRegistered Reviewers ({}):\n", reviewers.len()));
        for r in &reviewers {
            ctx.push_str(&format!("  - {r}\n"));
        }
        ctx.push_str("  Request formal reviews with verification action=request_review; do not message reviewers manually.\n");
    }

    // ── Orphan detection ──
    let orphaned: Vec<&serde_json::Map<String, Value>> = tasks
        .iter()
        .filter(|t| {
            let status = t.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let assignee = t.get("assignee").and_then(|v| v.as_str());
            !matches!(status, "closed" | "merged")
                && assignee.is_some_and(|a| !workers.contains(&a.to_string()))
        })
        .collect();

    if !orphaned.is_empty() {
        ctx.push_str("\n⚠ ORPHANED TASKS (assigned to workers that no longer exist):\n");
        for t in &orphaned {
            let id = t.get("task_id").and_then(|v| v.as_str()).unwrap_or("?");
            let assignee = t.get("assignee").and_then(|v| v.as_str()).unwrap_or("?");
            ctx.push_str(&format!(
                "  - {id} was assigned to '{assignee}' who is no longer registered.\n"
            ));
        }
        ctx.push_str(
            "  ACTION REQUIRED: Reassign these tasks to currently registered workers \
             using factory action=assign_workers.\n",
        );
    }

    ctx
}

/// Build a state summary for workers at session_start.
/// Shows any tasks already assigned to this worker.
fn build_worker_context(agent_name: &str) -> String {
    let Some(root) = brehon_root() else {
        return String::new();
    };

    let tasks_dir = root.join("runtime").join("tasks");
    let mut my_tasks: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&tasks_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(v) = serde_json::from_str::<Value>(&content) {
                    if v["assignee"].as_str() == Some(agent_name)
                        && !matches!(v["status"].as_str(), Some("closed" | "merged"))
                    {
                        let id = v["task_id"].as_str().unwrap_or("?");
                        let title = v["title"].as_str().unwrap_or("?");
                        let status = v["status"].as_str().unwrap_or("?");
                        my_tasks.push(format!("  - {id}: \"{title}\" [{status}]"));
                    }
                }
            }
        }
    }

    if my_tasks.is_empty() {
        String::new()
    } else {
        let mut ctx = format!("\n\n== YOUR ASSIGNED TASKS ({}) ==\n", my_tasks.len());
        for t in &my_tasks {
            ctx.push_str(t);
            ctx.push('\n');
        }
        ctx.push_str("Start working on these immediately. Call task action=mine for details.\n");
        ctx
    }
}

/// Write a per-agent session file so the TUI dashboard can discover registered agents.
///
/// Each MCP server process writes its own file at
/// `{brehon_root}/runtime/sessions/{name}.json` — no locking needed.
#[allow(dead_code)]
pub(crate) fn write_session_file(
    agent_name: &str,
    role: &str,
    session_id: &str,
    agent_type: Option<&str>,
) {
    write_session_file_with_metadata(agent_name, role, session_id, agent_type, None, None);
}

pub(crate) fn write_session_file_with_metadata(
    agent_name: &str,
    role: &str,
    session_id: &str,
    agent_type: Option<&str>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) {
    let Some(path) = session_path(agent_name) else {
        return;
    };
    let sessions_dir = path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    if std::fs::create_dir_all(&sessions_dir).is_err() {
        return;
    }

    let mut entry = build_session_entry(
        agent_name,
        role,
        session_id,
        agent_type,
        model,
        reasoning_effort,
    );

    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(existing) = serde_json::from_str::<Value>(&content) {
            if let Some(registered_at) = existing.get("registered_at").and_then(|v| v.as_str()) {
                entry["registered_at"] = Value::String(registered_at.to_string());
            }
            if entry.get("agent_type").is_none() {
                if let Some(existing_type) = existing.get("agent_type").and_then(|v| v.as_str()) {
                    entry["agent_type"] = Value::String(existing_type.to_string());
                }
            }
            if entry.get("model").is_none() {
                if let Some(existing_model) = existing.get("model").and_then(|v| v.as_str()) {
                    entry["model"] = Value::String(existing_model.to_string());
                }
            }
            if entry.get("reasoning_effort").is_none() {
                if let Some(existing_reasoning) =
                    existing.get("reasoning_effort").and_then(|v| v.as_str())
                {
                    entry["reasoning_effort"] = Value::String(existing_reasoning.to_string());
                }
            }
        }
    }
    entry["last_seen_at"] = Value::String(now_rfc3339());
    if let Ok(session_name) = std::env::var("BREHON_SESSION_NAME") {
        let session_name = session_name.trim();
        if !session_name.is_empty() {
            entry["session_name"] = Value::String(session_name.to_string());
        }
    }

    // Atomic write: temp file then rename
    let tmp = sessions_dir.join(format!(".{agent_name}.tmp"));
    if let Ok(data) = serde_json::to_string_pretty(&entry) {
        if std::fs::write(&tmp, &data).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn build_session_entry(
    agent_name: &str,
    role: &str,
    session_id: &str,
    agent_type: Option<&str>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> Value {
    let now = now_rfc3339();
    let mut entry = serde_json::json!({
        "name": agent_name,
        "role": role,
        "session_id": session_id,
        "registered_at": now,
        "last_seen_at": now_rfc3339(),
    });
    if let Some(agent_type) = agent_type {
        if !agent_type.is_empty() {
            entry["agent_type"] = serde_json::Value::String(agent_type.to_string());
        }
    }
    if let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) {
        entry["model"] = serde_json::Value::String(model.to_string());
    }
    if let Some(reasoning_effort) = reasoning_effort
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        entry["reasoning_effort"] = serde_json::Value::String(reasoning_effort.to_string());
    }
    entry
}

/// MCP tool for agent lifecycle management (session start, identity, messaging).
pub struct AgentTool;

impl Default for AgentTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentTool {
    /// Create a new agent lifecycle tool instance.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        "agent"
    }

    fn description(&self) -> &str {
        "Agent lifecycle management - session registration, identity, and messaging. \
         Call with action=session_start on startup to register and receive instructions."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action: session_start, whoami, message, delivery_status"
                },
                "name": {
                    "type": "string",
                    "description": "Agent name (for session_start)"
                },
                "agent_type": {
                    "type": "string",
                    "description": "Agent type: supervisor, worker, reviewer, advisor, research (for session_start)"
                },
                "target": {
                    "type": "string",
                    "description": "Target agent name (for message, or optional filter for delivery_status)"
                },
                "message": {
                    "type": "string",
                    "description": "Message content (for message)"
                },
                "prompt_id": {
                    "type": "string",
                    "description": "For action=delivery_status: the prompt_id returned by a prior action=message call. Returns queued/injected/dead_lettered/drained_without_ack state plus timestamps so the caller can distinguish prompts not yet delivered from prompts injected into the target pane."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, McpError> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");

        match action {
            "session_start" => {
                let agent_name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| std::env::var("BREHON_AGENT_NAME").ok())
                    .unwrap_or_else(|| "unknown".to_string());

                let role = args
                    .get("agent_type")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| std::env::var("BREHON_AGENT_ROLE").ok())
                    .unwrap_or_else(|| "worker".to_string());

                let session_id = std::env::var("BREHON_SESSION_ID")
                    .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());

                let supervisor_name =
                    resolve_supervisor_name(None).unwrap_or_else(|| "supervisor".to_string());
                let resolved_agent_type = std::env::var("BREHON_AGENT_TYPE").ok();
                let resolved_model = std::env::var("BREHON_AGENT_MODEL").ok();
                let resolved_reasoning_effort = std::env::var("BREHON_REASONING_EFFORT").ok();
                let config = load_project_config();
                let project_policy = config
                    .as_ref()
                    .and_then(|cfg| cfg.project_prompt_for_role_name(role.as_str()));

                // Build role-specific instructions + state context
                let (instructions, context) = match role.as_str() {
                    "worker" => (
                        worker_instructions(
                            &agent_name,
                            &supervisor_name,
                            project_policy.as_deref(),
                        ),
                        build_worker_context(&agent_name),
                    ),
                    "supervisor" => (
                        supervisor_instructions(
                            &agent_name,
                            config
                                .as_ref()
                                .and_then(|cfg| cfg.roles.supervisor.system_prompt.as_deref()),
                            project_policy.as_deref(),
                        ),
                        build_supervisor_context(),
                    ),
                    "reviewer" => (
                        reviewer_instructions(
                            &agent_name,
                            config
                                .as_ref()
                                .and_then(|cfg| {
                                    configured_reviewer_prompt(cfg, resolved_agent_type.as_deref())
                                })
                                .as_deref(),
                            project_policy.as_deref(),
                            config
                                .as_ref()
                                .map(|cfg| cfg.review.lease_mode)
                                .unwrap_or_default(),
                        ),
                        String::new(),
                    ),
                    "advisor" => (
                        advisor_instructions(
                            &agent_name,
                            config
                                .as_ref()
                                .and_then(|cfg| {
                                    configured_advisor_prompt(cfg, resolved_agent_type.as_deref())
                                })
                                .as_deref(),
                            project_policy.as_deref(),
                        ),
                        String::new(),
                    ),
                    "research" => (
                        research_instructions(
                            &agent_name,
                            config
                                .as_ref()
                                .and_then(|cfg| {
                                    configured_research_prompt(cfg, resolved_agent_type.as_deref())
                                })
                                .as_deref(),
                            project_policy.as_deref(),
                        ),
                        String::new(),
                    ),
                    _ => (
                        format!(
                            "Unknown role '{}'. Call agent action=whoami for identity.",
                            role
                        ),
                        String::new(),
                    ),
                };

                // Persist registration so the TUI dashboard can discover this agent.
                write_session_file_with_metadata(
                    &agent_name,
                    &role,
                    &session_id,
                    resolved_agent_type.as_deref(),
                    resolved_model.as_deref(),
                    resolved_reasoning_effort.as_deref(),
                );

                // Combine instructions with recovered state context
                let full_instructions = if context.is_empty() {
                    instructions
                } else {
                    format!("{instructions}{context}")
                };

                let result = serde_json::json!({
                    "status": "ok",
                    "session_id": session_id,
                    "agent_name": agent_name,
                    "role": role,
                    "agent_type": resolved_agent_type,
                    "model": resolved_model,
                    "reasoning_effort": resolved_reasoning_effort,
                    "instructions": full_instructions
                });

                Ok(text_result(
                    serde_json::to_string_pretty(&result)
                        .map_err(|e| McpError::Serialization(e.to_string()))?,
                ))
            }
            "whoami" => {
                let agent_name =
                    std::env::var("BREHON_AGENT_NAME").unwrap_or_else(|_| "unknown".to_string());
                let role =
                    std::env::var("BREHON_AGENT_ROLE").unwrap_or_else(|_| "unknown".to_string());
                let session_id =
                    std::env::var("BREHON_SESSION_ID").unwrap_or_else(|_| "unknown".to_string());
                let supervisor_name = resolve_supervisor_name(None);
                let agent_type = std::env::var("BREHON_AGENT_TYPE").ok();
                let model = std::env::var("BREHON_AGENT_MODEL").ok();
                let reasoning_effort = std::env::var("BREHON_REASONING_EFFORT").ok();

                let result = serde_json::json!({
                    "agent_name": agent_name,
                    "role": role,
                    "session_id": session_id,
                    "supervisor": supervisor_name,
                    "agent_type": agent_type,
                    "model": model,
                    "reasoning_effort": reasoning_effort
                });

                Ok(text_result(
                    serde_json::to_string_pretty(&result)
                        .map_err(|e| McpError::Serialization(e.to_string()))?,
                ))
            }
            "message" => {
                let target = args
                    .get("target")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
                let from =
                    std::env::var("BREHON_AGENT_NAME").unwrap_or_else(|_| "unknown".to_string());

                let outcome = try_deliver_message(target, &from, message);

                let mut result = serde_json::json!({
                    "status": if outcome.queued { "delivered" } else { "failed" },
                    "method": outcome.method,
                    "target": target,
                    "message": message
                });
                if !outcome.prompt_id.is_empty() {
                    result["prompt_id"] = Value::String(outcome.prompt_id);
                }

                Ok(text_result(
                    serde_json::to_string_pretty(&result)
                        .map_err(|e| McpError::Serialization(e.to_string()))?,
                ))
            }
            "delivery_status" => {
                let prompt_id = args
                    .get("prompt_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                let Some(prompt_id) = prompt_id else {
                    return Ok(error_result(
                        "Missing required parameter: prompt_id. Pass the prompt_id returned by a prior `agent action=message` call."
                            .to_string(),
                    ));
                };
                let status = resolve_delivery_status(prompt_id);
                let result = serde_json::json!({
                    "status": "ok",
                    "prompt_id": prompt_id,
                    "enqueued": status.enqueued,
                    "enqueued_at": status.enqueued_at,
                    "queued": status.queued,
                    "injected": status.injected,
                    "injected_at": status.injected_at,
                    "injected_method": status.injected_method,
                    "target": status.target,
                    "dead_lettered": status.dead_lettered,
                    "dead_letter_reason": status.dead_letter_reason,
                    "overall": status.overall,
                    "message": status.human_summary,
                });
                Ok(text_result(
                    serde_json::to_string_pretty(&result)
                        .map_err(|e| McpError::Serialization(e.to_string()))?,
                ))
            }
            _ => Ok(error_result(format!("Unknown agent action: {}", action))),
        }
    }
}

/// Aggregated view of what we know about a given `prompt_id` on disk. Each
/// field is independently observable — the supervisor can distinguish "the
/// MCP didn't even enqueue this" from "the TUI enqueued but never injected"
/// from "the TUI injected and the worker went silent" from "the TUI failed
/// and dead-lettered the prompt".
#[derive(Debug, Default)]
struct DeliveryStatus {
    enqueued: bool,
    enqueued_at: Option<String>,
    queued: bool,
    injected: bool,
    injected_at: Option<String>,
    injected_method: Option<String>,
    target: Option<String>,
    dead_lettered: bool,
    dead_letter_reason: Option<String>,
    overall: &'static str,
    human_summary: String,
}

fn resolve_delivery_status(prompt_id: &str) -> DeliveryStatus {
    let mut status = DeliveryStatus::default();
    let Some(root) = brehon_root() else {
        status.overall = "unknown_no_brehon_root";
        status.human_summary =
            "BREHON_ROOT is not set; cannot resolve delivery acks without it.".to_string();
        return status;
    };

    // 1. Enqueue ack — the MCP writer records this immediately after the
    //    prompt-queue write succeeds. If the queue file later disappears without
    //    an injection ack or dead-letter, this lets the supervisor distinguish
    //    "drained without ack" from "never enqueued".
    let enqueue_path = prompt_enqueue_ack_dir(&root)
        .join(format!("{}.json", sanitize_prompt_id_for_path(prompt_id)));
    if let Ok(content) = std::fs::read_to_string(&enqueue_path) {
        if let Ok(value) = serde_json::from_str::<Value>(&content) {
            status.enqueued = true;
            status.enqueued_at = value
                .get("queued_at")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            status.target = value
                .get("target")
                .and_then(|v| v.as_str())
                .map(str::to_string);
        }
    }

    // 2. Injection ack — the TUI writes this after a successful pane inject.
    let ack_path = root
        .join("runtime")
        .join("prompt-delivery-acks")
        .join(format!("{}.json", sanitize_prompt_id_for_path(prompt_id)));
    if let Ok(content) = std::fs::read_to_string(&ack_path) {
        if let Ok(value) = serde_json::from_str::<Value>(&content) {
            status.injected = true;
            status.injected_at = value
                .get("injected_at")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            status.injected_method = value
                .get("method")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            status.target = value
                .get("target")
                .and_then(|v| v.as_str())
                .map(str::to_string);
        }
    }

    // 3. Queue presence — if the prompt file still exists in the prompt-queue,
    //    the TUI hasn't yet consumed it (or is retrying). If the ack is also
    //    absent, the prompt is still in flight.
    let queue_dir = root.join("runtime").join("prompt-queue");
    let mut queue_hit = false;
    if let Ok(entries) = std::fs::read_dir(&queue_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if let Ok(nested) = std::fs::read_dir(&p) {
                    for inner in nested.flatten() {
                        if prompt_file_matches_id(inner.path().as_path(), prompt_id) {
                            queue_hit = true;
                            break;
                        }
                    }
                }
                if queue_hit {
                    break;
                }
            } else if prompt_file_matches_id(p.as_path(), prompt_id) {
                queue_hit = true;
                break;
            }
        }
    }
    status.queued = queue_hit;

    // 4. Dead-letter — nonrecoverable delivery failure.
    let dead_letter_dir = root.join("runtime").join("prompt-dead-letter");
    if let Ok(entries) = std::fs::read_dir(&dead_letter_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            let hit = if p.is_dir() {
                std::fs::read_dir(&p)
                    .map(|nested| {
                        nested
                            .flatten()
                            .any(|inner| prompt_file_matches_id(inner.path().as_path(), prompt_id))
                    })
                    .unwrap_or(false)
            } else {
                prompt_file_matches_id(p.as_path(), prompt_id)
            };
            if hit {
                status.dead_lettered = true;
                if !p.is_dir() {
                    if let Ok(content) = std::fs::read_to_string(&p) {
                        if let Ok(value) = serde_json::from_str::<Value>(&content) {
                            let source = value.get("entry").unwrap_or(&value);
                            status.dead_letter_reason = source
                                .get("reason")
                                .and_then(|v| v.as_str())
                                .map(str::to_string);
                        }
                    }
                }
                break;
            }
        }
    }

    status.overall = if status.dead_lettered {
        "dead_lettered"
    } else if status.injected {
        "injected"
    } else if status.queued {
        "queued"
    } else if status.enqueued {
        "drained_without_ack"
    } else {
        "unknown"
    };
    status.human_summary = match status.overall {
        "injected" => format!(
            "Prompt {prompt_id} was injected into {} at {}. If the worker has not responded, they received it but chose (or failed) to act.",
            status.target.as_deref().unwrap_or("?"),
            status.injected_at.as_deref().unwrap_or("?"),
        ),
        "queued" => format!(
            "Prompt {prompt_id} is still in the prompt queue; the TUI has not yet injected it. Worker has not seen it."
        ),
        "dead_lettered" => format!(
            "Prompt {prompt_id} was dead-lettered: {}. The worker never saw it and never will — resend or investigate.",
            status.dead_letter_reason.as_deref().unwrap_or("reason not recorded"),
        ),
        "drained_without_ack" => format!(
            "Prompt {prompt_id} was enqueued at {}, but is no longer in the queue and has no injection ack or dead-letter. Treat delivery as uncertain and resend after checking the TUI.",
            status.enqueued_at.as_deref().unwrap_or("?"),
        ),
        _ => format!(
            "Prompt {prompt_id} is not present in queue, injection-ack, or dead-letter. It may never have been enqueued, or the records have been pruned."
        ),
    };

    status
}

fn prompt_file_matches_id(path: &std::path::Path, prompt_id: &str) -> bool {
    if path.is_dir() {
        return false;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&content) else {
        return false;
    };
    if value.get("prompt_id").and_then(|v| v.as_str()) == Some(prompt_id) {
        return true;
    }
    value
        .get("entry")
        .and_then(|entry| entry.get("prompt_id"))
        .and_then(|v| v.as_str())
        == Some(prompt_id)
}

fn sanitize_prompt_id_for_path(prompt_id: &str) -> String {
    // Mirrors `sanitize_runtime_key` in the TUI helpers — same character set
    // so the MCP can reconstruct the ack filename the TUI writes.
    let mut out = String::with_capacity(prompt_id.len());
    for ch in prompt_id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::ContentBlock;
    use crate::tools::TEST_ENV_LOCK;
    use std::ffi::OsString;

    struct ScopedEnv {
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl ScopedEnv {
        fn set(vars: &[(&'static str, &str)]) -> Self {
            let mut all_vars: Vec<(&'static str, &str)> = vars.to_vec();
            let auto_clear = [
                "BREHON_SESSION_NAME",
                "BREHON_WORKTREE_BRANCH",
                "BREHON_SUPERVISOR_NAME",
            ];
            for &key in &auto_clear {
                if !all_vars.iter().any(|(k, _)| *k == key) {
                    all_vars.push((key, ""));
                }
            }
            let mut saved = Vec::with_capacity(all_vars.len());
            for (key, value) in &all_vars {
                saved.push((*key, std::env::var_os(key)));
                std::env::set_var(key, value);
            }
            Self { saved }
        }
    }

    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            for (key, value) in self.saved.iter().rev() {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }

    fn write_current_session(root: &std::path::Path, session_name: &str) {
        let runtime_dir = root.join("runtime");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::write(
            runtime_dir.join("current-session.json"),
            serde_json::json!({
                "session_name": session_name,
                "written_at": "2026-04-22T14:00:00+00:00"
            })
            .to_string(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn test_session_start_worker() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = ScopedEnv::set(&[
            ("BREHON_ROOT", root.path().to_str().unwrap()),
            ("BREHON_AGENT_TYPE", "codex"),
        ]);

        let tool = AgentTool::new();
        let args = serde_json::json!({
            "action": "session_start",
            "name": "worker-1",
            "agent_type": "worker"
        });
        let result = tool.execute(args).await.unwrap();
        assert!(result.is_error.is_none());
        if let ContentBlock::Text { text } = &result.content[0] {
            let v: Value = serde_json::from_str(text).unwrap();
            assert_eq!(v["status"], "ok");
            assert_eq!(v["agent_name"], "worker-1");
            assert_eq!(v["role"], "worker");
            // session_start returns structured instructions
            let instructions = v["instructions"].as_str().unwrap();
            assert!(instructions.contains("worker-1"));
            assert!(instructions.contains("action=whoami"));
            assert!(instructions.contains("assigned` to `in_progress"));
            assert!(instructions.contains("task action=complete id=<task>"));
            assert!(instructions.contains("`review_ready`"));
            assert!(instructions.contains("notifies the supervisor"));
        }
    }

    #[tokio::test]
    async fn test_session_start_worker_uses_live_supervisor_when_env_missing() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = ScopedEnv::set(&[
            ("BREHON_ROOT", root.path().to_str().unwrap()),
            ("BREHON_SUPERVISOR_NAME", ""),
            ("BREHON_WORKTREE_BRANCH", ""),
        ]);

        write_session_file("claude-code", "supervisor", "sup-session", Some("claude"));

        let tool = AgentTool::new();
        let args = serde_json::json!({
            "action": "session_start",
            "name": "worker-1",
            "agent_type": "worker"
        });
        let result = tool.execute(args).await.unwrap();
        assert!(result.is_error.is_none());
        if let ContentBlock::Text { text } = &result.content[0] {
            let v: Value = serde_json::from_str(text).unwrap();
            let instructions = v["instructions"].as_str().unwrap();
            assert!(instructions.contains("Your supervisor is 'claude-code'"));
            assert!(instructions.contains("target=claude-code"));
        }
    }

    #[tokio::test]
    async fn test_session_start_research() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = ScopedEnv::set(&[
            ("BREHON_ROOT", root.path().to_str().unwrap()),
            ("BREHON_AGENT_TYPE", "spec-research"),
        ]);

        let tool = AgentTool::new();
        let result = tool
            .execute(serde_json::json!({
                "action": "session_start",
                "name": "research-1",
                "agent_type": "research"
            }))
            .await
            .unwrap();
        assert!(result.is_error.is_none());
        if let ContentBlock::Text { text } = &result.content[0] {
            let v: Value = serde_json::from_str(text).unwrap();
            assert_eq!(v["status"], "ok");
            assert_eq!(v["role"], "research");
            let instructions = v["instructions"].as_str().unwrap();
            assert!(instructions.contains("research action=claim_next"));
            assert!(instructions.contains("research action=submit"));
            assert!(instructions.contains("read-only"));
        }
    }

    #[tokio::test]
    async fn test_session_start_supervisor() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = ScopedEnv::set(&[
            ("BREHON_ROOT", root.path().to_str().unwrap()),
            ("BREHON_AGENT_TYPE", "claude-code"),
        ]);

        let tool = AgentTool::new();
        let args = serde_json::json!({
            "action": "session_start",
            "name": "sup-1",
            "agent_type": "supervisor"
        });
        let result = tool.execute(args).await.unwrap();
        assert!(result.is_error.is_none());
        if let ContentBlock::Text { text } = &result.content[0] {
            let v: Value = serde_json::from_str(text).unwrap();
            assert_eq!(v["role"], "supervisor");
            let instructions = v["instructions"].as_str().unwrap();
            assert!(instructions.contains("supervisor"));
            assert!(instructions.contains("factory action="));
            assert!(instructions.contains("completion_mode=close"));
            assert!(instructions.contains("Audit/no-code tasks should"));
            assert!(instructions.contains("task action=integrate"));
            assert!(instructions.contains("Only the final epic close"));
            assert!(instructions.contains("search_skills query=\"\""));
            assert!(instructions.contains("search_rules query=\"\""));
        }
    }

    #[tokio::test]
    async fn test_session_start_supervisor_includes_configured_system_prompt() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let project_root = tempfile::tempdir().unwrap();
        let brehon_root = project_root.path().join(".brehon");
        std::fs::create_dir_all(&brehon_root).unwrap();

        let mut config = brehon_config::parse_defaults().unwrap();
        config.roles.supervisor.system_prompt =
            Some("Configured supervisor prompt from project config.".to_string());
        std::fs::write(
            brehon_root.join("config.yaml"),
            serde_yaml::to_string(&config).unwrap(),
        )
        .unwrap();

        let _env = ScopedEnv::set(&[
            ("BREHON_ROOT", brehon_root.to_str().unwrap()),
            ("BREHON_AGENT_TYPE", "claude-code"),
        ]);

        let tool = AgentTool::new();
        let args = serde_json::json!({
            "action": "session_start",
            "name": "sup-1",
            "agent_type": "supervisor"
        });
        let result = tool.execute(args).await.unwrap();
        assert!(result.is_error.is_none());
        if let ContentBlock::Text { text } = &result.content[0] {
            let v: Value = serde_json::from_str(text).unwrap();
            let instructions = v["instructions"].as_str().unwrap();
            assert!(instructions.contains("Configured supervisor prompt from project config."));
            assert!(instructions.contains("Hard Rules:"));
            assert!(instructions.contains("task action=conflicts"));
            assert!(instructions.contains("highest-priority unblockers"));
        }
    }

    #[tokio::test]
    async fn test_session_start_reviewer() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = ScopedEnv::set(&[
            ("BREHON_ROOT", root.path().to_str().unwrap()),
            ("BREHON_AGENT_TYPE", "codex"),
        ]);

        let tool = AgentTool::new();
        let args = serde_json::json!({
            "action": "session_start",
            "name": "rev-1",
            "agent_type": "reviewer"
        });
        let result = tool.execute(args).await.unwrap();
        assert!(result.is_error.is_none());
        if let ContentBlock::Text { text } = &result.content[0] {
            let v: Value = serde_json::from_str(text).unwrap();
            assert_eq!(v["role"], "reviewer");
            let instructions = v["instructions"].as_str().unwrap();
            assert!(instructions.contains("reviewer"));
            assert!(instructions.contains("leased panels"));
            assert!(instructions.contains("changes_requested"));
            assert!(instructions.contains("submit_review"));
            assert!(instructions.contains("review_obligations"));
            assert!(instructions.contains("END THE TURN"));
            assert!(instructions.contains("separate prompts"));
        }
    }

    #[tokio::test]
    async fn test_session_start_reviewer_includes_configured_pool_prompt() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let project_root = tempfile::tempdir().unwrap();
        let brehon_root = project_root.path().join(".brehon");
        std::fs::create_dir_all(&brehon_root).unwrap();

        let mut config = brehon_config::parse_defaults().unwrap();
        let reviewer = config
            .roles
            .reviewers
            .iter_mut()
            .find(|reviewer| reviewer.lane == "codex-reviewer")
            .unwrap();
        reviewer.system_prompt = Some(
            "Configured reviewer prompt: active review obligations are authoritative.".to_string(),
        );
        std::fs::write(
            brehon_root.join("config.yaml"),
            serde_yaml::to_string(&config).unwrap(),
        )
        .unwrap();

        let _env = ScopedEnv::set(&[
            ("BREHON_ROOT", brehon_root.to_str().unwrap()),
            ("BREHON_AGENT_TYPE", "codex-reviewer"),
        ]);

        let tool = AgentTool::new();
        let args = serde_json::json!({
            "action": "session_start",
            "name": "rev-1",
            "agent_type": "reviewer"
        });
        let result = tool.execute(args).await.unwrap();
        assert!(result.is_error.is_none());
        if let ContentBlock::Text { text } = &result.content[0] {
            let v: Value = serde_json::from_str(text).unwrap();
            let instructions = v["instructions"].as_str().unwrap();
            assert!(instructions.contains(
                "Configured reviewer prompt: active review obligations are authoritative."
            ));
            assert!(instructions.contains("leased panels"));
            assert!(instructions.contains("review_obligations"));
            assert!(instructions.contains("submit_review"));
        }
    }

    #[tokio::test]
    async fn test_session_start_reviewer_reflects_shared_after_submit_policy() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let project_root = tempfile::tempdir().unwrap();
        let brehon_root = project_root.path().join(".brehon");
        std::fs::create_dir_all(&brehon_root).unwrap();

        let mut config = brehon_config::parse_defaults().unwrap();
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        std::fs::write(
            brehon_root.join("config.yaml"),
            serde_yaml::to_string(&config).unwrap(),
        )
        .unwrap();

        let _env = ScopedEnv::set(&[
            ("BREHON_ROOT", brehon_root.to_str().unwrap()),
            ("BREHON_AGENT_TYPE", "codex-reviewer"),
        ]);

        let tool = AgentTool::new();
        let args = serde_json::json!({
            "action": "session_start",
            "name": "rev-1",
            "agent_type": "reviewer"
        });
        let result = tool.execute(args).await.unwrap();
        assert!(result.is_error.is_none());
        if let ContentBlock::Text { text } = &result.content[0] {
            let v: Value = serde_json::from_str(text).unwrap();
            let instructions = v["instructions"].as_str().unwrap();
            assert!(instructions.contains("shared-after-submit panels"));
            assert!(instructions.contains("hard-resets your reviewer session"));
        }
    }

    #[test]
    fn test_build_session_entry_includes_agent_metadata_when_present() {
        let typed = build_session_entry(
            "rev-1",
            "reviewer",
            "sess-1",
            Some("codex"),
            Some("gpt-5.4"),
            Some("xhigh"),
        );
        assert_eq!(typed["name"], "rev-1");
        assert_eq!(typed["role"], "reviewer");
        assert_eq!(typed["session_id"], "sess-1");
        assert_eq!(typed["agent_type"], "codex");
        assert_eq!(typed["model"], "gpt-5.4");
        assert_eq!(typed["reasoning_effort"], "xhigh");
        assert!(typed.get("registered_at").is_some());

        let legacy = build_session_entry("rev-2", "reviewer", "sess-2", None, None, None);
        assert_eq!(legacy["name"], "rev-2");
        assert_eq!(legacy["role"], "reviewer");
        assert_eq!(legacy["session_id"], "sess-2");
        assert!(legacy.get("agent_type").is_none());
        assert!(legacy.get("model").is_none());
        assert!(legacy.get("reasoning_effort").is_none());
    }

    #[tokio::test]
    async fn test_session_start_persists_model_metadata() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = ScopedEnv::set(&[
            ("BREHON_ROOT", root.path().to_str().unwrap()),
            ("BREHON_AGENT_TYPE", "codex-fast"),
            ("BREHON_AGENT_MODEL", "gpt-5.4"),
            ("BREHON_REASONING_EFFORT", "medium"),
        ]);

        let tool = AgentTool::new();
        let args = serde_json::json!({
            "action": "session_start",
            "name": "worker-1",
            "agent_type": "worker"
        });
        let result = tool.execute(args).await.unwrap();
        assert!(result.is_error.is_none());
        if let ContentBlock::Text { text } = &result.content[0] {
            let response: Value = serde_json::from_str(text).unwrap();
            assert_eq!(response["agent_type"], "codex-fast");
            assert_eq!(response["model"], "gpt-5.4");
            assert_eq!(response["reasoning_effort"], "medium");
        }

        let session_file = root
            .path()
            .join("runtime")
            .join("sessions")
            .join("worker-1.json");
        let session: Value =
            serde_json::from_str(&std::fs::read_to_string(session_file).unwrap()).unwrap();
        assert_eq!(session["agent_type"], "codex-fast");
        assert_eq!(session["model"], "gpt-5.4");
        assert_eq!(session["reasoning_effort"], "medium");
    }

    #[tokio::test]
    async fn test_whoami() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = ScopedEnv::set(&[
            ("BREHON_AGENT_NAME", "worker-1"),
            ("BREHON_AGENT_ROLE", "worker"),
            ("BREHON_SESSION_ID", "session-1"),
            ("BREHON_AGENT_TYPE", "deepseek-claude-reviewer"),
            ("BREHON_AGENT_MODEL", "deepseek-v4-pro"),
            ("BREHON_REASONING_EFFORT", "max"),
        ]);

        let tool = AgentTool::new();
        let args = serde_json::json!({ "action": "whoami" });
        let result = tool.execute(args).await.unwrap();
        assert!(result.is_error.is_none());
        if let ContentBlock::Text { text } = &result.content[0] {
            let v: Value = serde_json::from_str(text).unwrap();
            assert_eq!(v["agent_name"], "worker-1");
            assert_eq!(v["role"], "worker");
            assert_eq!(v["session_id"], "session-1");
            assert_eq!(v["agent_type"], "deepseek-claude-reviewer");
            assert_eq!(v["model"], "deepseek-v4-pro");
            assert_eq!(v["reasoning_effort"], "max");
        }
    }

    #[tokio::test]
    async fn test_message() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = ScopedEnv::set(&[
            ("BREHON_ROOT", root.path().to_str().unwrap()),
            ("BREHON_AGENT_NAME", "worker-1"),
        ]);

        let tool = AgentTool::new();
        let args = serde_json::json!({
            "action": "message",
            "target": "supervisor",
            "message": "Task complete"
        });
        let result = tool.execute(args).await.unwrap();
        assert!(result.is_error.is_none());
        if let ContentBlock::Text { text } = &result.content[0] {
            let v: Value = serde_json::from_str(text).unwrap();
            assert_eq!(v["status"], "delivered");
            assert_eq!(v["method"], "queued");
            assert_eq!(v["target"], "supervisor");
            assert_eq!(v["message"], "Task complete");
        }
    }

    #[test]
    fn test_try_deliver_message_queues_multiple_messages_for_same_target() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = ScopedEnv::set(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
        write_current_session(root.path(), "brehon-test");

        let first = try_deliver_message("supervisor", "worker-1", "first message");
        let second = try_deliver_message("supervisor", "worker-2", "second message");

        assert!(first.queued);
        assert!(second.queued);
        assert!(!first.prompt_id.is_empty());
        assert!(!second.prompt_id.is_empty());
        assert_ne!(first.prompt_id, second.prompt_id);

        let prompt_queue = SessionScopedQueue::<PromptQueueEntry>::new(
            "brehon-test",
            prompt_queue_root(root.path()),
        );
        let drained: Vec<_> = prompt_queue.drain().collect();
        assert_eq!(drained.len(), 2, "messages must queue, not overwrite");

        let mut payloads = Vec::new();
        for result in drained {
            let value = result.expect("prompt entry should decode");
            assert_eq!(value.session_name, "brehon-test");
            assert_eq!(value.entry.target, "supervisor");
            assert!(matches!(
                value.entry.from.as_deref(),
                Some("worker-1" | "worker-2")
            ));
            payloads.push(value.entry.message);
        }

        assert!(payloads.contains(&"first message".to_string()));
        assert!(payloads.contains(&"second message".to_string()));
    }

    #[test]
    fn test_try_deliver_message_stamps_resolved_session_into_payload() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = ScopedEnv::set(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
        write_current_session(root.path(), "brehon-disk-resolved");

        let outcome = try_deliver_message("supervisor", "worker-1", "hello");
        assert!(outcome.queued);
        assert!(!outcome.prompt_id.is_empty());

        let prompt_queue = SessionScopedQueue::<PromptQueueEntry>::new(
            "brehon-disk-resolved",
            prompt_queue_root(root.path()),
        );
        let drained: Vec<_> = prompt_queue.drain().collect();
        assert_eq!(drained.len(), 1);
        let payload = drained[0].as_ref().expect("prompt entry should decode");
        assert_eq!(payload.session_name, "brehon-disk-resolved");
        assert_eq!(payload.entry.target, "supervisor");
        assert_eq!(payload.entry.from.as_deref(), Some("worker-1"));
        assert_eq!(payload.entry.message, "hello");
        assert_eq!(
            payload.entry.prompt_id.as_deref(),
            Some(outcome.prompt_id.as_str())
        );
    }

    #[test]
    fn test_delivery_status_finds_session_scoped_prompt_queue_entry() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = ScopedEnv::set(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
        write_current_session(root.path(), "brehon-disk-resolved");

        let outcome = try_deliver_message("supervisor", "worker-1", "hello");
        assert!(outcome.queued);

        let status = resolve_delivery_status(&outcome.prompt_id);
        assert!(status.enqueued, "enqueue ack should be visible");
        assert!(status.queued, "scoped .entry prompt should be visible");
        assert_eq!(status.overall, "queued");
    }

    #[test]
    fn test_delivery_status_reports_drained_without_ack() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = ScopedEnv::set(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
        write_current_session(root.path(), "brehon-disk-resolved");

        let outcome = try_deliver_message("supervisor", "worker-1", "hello");
        assert!(outcome.queued);

        let prompt_queue = SessionScopedQueue::<PromptQueueEntry>::new(
            "brehon-disk-resolved",
            prompt_queue_root(root.path()),
        );
        let drained: Vec<_> = prompt_queue.drain().collect();
        assert_eq!(drained.len(), 1);

        let status = resolve_delivery_status(&outcome.prompt_id);
        assert!(status.enqueued);
        assert!(!status.queued);
        assert!(!status.injected);
        assert!(!status.dead_lettered);
        assert_eq!(status.overall, "drained_without_ack");
    }

    #[test]
    fn test_try_deliver_message_uses_missing_runtime_session_sentinel_when_unresolved() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = ScopedEnv::set(&[("BREHON_ROOT", root.path().to_str().unwrap())]);

        // current-session.json is intentionally absent.
        let outcome = try_deliver_message("supervisor", "worker-1", "orphan");
        assert!(outcome.queued);

        let prompt_queue = SessionScopedQueue::<PromptQueueEntry>::new(
            "__missing-runtime-session__",
            prompt_queue_root(root.path()),
        );
        let drained: Vec<_> = prompt_queue.drain().collect();
        assert_eq!(
            drained.len(),
            1,
            "message should still enqueue with explicit scope"
        );
        let payload = drained[0].as_ref().expect("prompt entry should decode");
        assert_eq!(payload.session_name, "__missing-runtime-session__");
        assert_eq!(payload.entry.target, "supervisor");
        assert_eq!(payload.entry.from.as_deref(), Some("worker-1"));
        assert_eq!(payload.entry.message, "orphan");
    }

    #[tokio::test]
    async fn test_unknown_action() {
        let tool = AgentTool::new();
        let args = serde_json::json!({ "action": "bogus" });
        let result = tool.execute(args).await.unwrap();
        assert_eq!(result.is_error, Some(true));
    }
}

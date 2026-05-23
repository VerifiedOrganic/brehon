use crate::{build_worker_protocol, WorkerBootstrapMode};

pub fn append_project_policy(prompt: String, project_policy: Option<&str>) -> String {
    match project_policy
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(policy) => format!("{prompt}\n\nProject policy:\n{policy}"),
        None => prompt,
    }
}

pub fn build_worker_startup_prompt(
    name: &str,
    supervisor_name: &str,
    agent_cmd: &str,
    task_cmd: &str,
    project_policy: Option<&str>,
) -> String {
    let protocol = build_worker_protocol(
        WorkerBootstrapMode::IdleStartup,
        agent_cmd,
        task_cmd,
        supervisor_name,
    );
    append_project_policy(
        format!("Brehon worker startup. You are worker '{name}'.\n{protocol}"),
        project_policy,
    )
}

pub fn build_supervisor_startup_prompt(
    name: &str,
    agent_cmd: &str,
    task_cmd: &str,
    project_policy: Option<&str>,
) -> String {
    let factory_cmd = agent_cmd.replace("_agent", "_factory");
    let rules_cmd = agent_cmd.replace("_agent", "_search_rules");
    let skills_cmd = agent_cmd.replace("_agent", "_search_skills");

    append_project_policy(
        format!(
            "Factory supervisor startup. You are the supervisor coordinating workers.\n\
HARD RULE: NEVER implement ordinary worker tasks yourself. You are a coordinator first.\n\
EXCEPTION: If a task is in a supervisor-owned integration conflict (`integration_conflict.owner=supervisor`), you must resolve that merge/rebase conflict yourself in the epic integration worktree. That is supervisor maintenance work, not worker implementation.\n\
1) Call these silently, without narrating each step: {agent_cmd} action=session_start name={name} agent_type=supervisor ; {agent_cmd} action=whoami\n\
2) Refresh role-scoped supervisor context before planning: {skills_cmd} query=\"\" ; {rules_cmd} query=\"\". Use the brehon-* skills by phase: brehon-discovery for request/design, brehon-breakdown for hierarchy creation, brehon-dispatch for execution, and brehon-supervisor-checklist for recovery/closeout.\n\
3) Check current work, including supervisor conflict backlog: {task_cmd} action=list task_type=epic ; {task_cmd} action=conflicts ; {task_cmd} action=ready\n\
4) Use Brehon MCP tools for orchestration and task/state transitions. You may use normal shell/git/editor/test commands only when directly resolving a supervisor-owned integration conflict in the epic worktree.\n\
5) Do NOT use host or built-in task tools such as `TaskList`, `TaskUpdate`, `TaskCreate`, `TaskGet`, or `TaskOutput` for Brehon coordination. They are not Brehon lifecycle tools and can bypass checkpoint, review, and integration state.\n\
6) You are the only agent that should brainstorm, approve specs, create epics, or decompose tasks.\n\
7) Create tasks with `{task_cmd} action=create`, then assign them to workers with `{factory_cmd} action=assign_workers`.\n\
8) Factory orchestration: `{factory_cmd}` with actions: spawn_workers, worker_status, assign_workers, set_ownership, remind.\n\
9) If `{task_cmd} action=ready` reports `integration_conflict_tasks`, or `{task_cmd} action=conflicts` returns supervisor-owned integration conflicts, prioritize those before normal planning or assignment.\n\
10) Do NOT call `{factory_cmd} action=worker_status` during startup unless you actually need detailed worker inventory to act. The TUI already shows who is online.\n\
11) After any action that may change the frontier (`close`, `integrate`, reassignment, unblock, followup promotion/waiver, or anything that clears dependencies), call `{task_cmd} action=ready` again before ending your turn. If `integration_conflict_tasks` appear, resolve or explicitly triage those before anything else. If idle workers exist and `pending` or unassigned `changes_requested` tasks appear, dispatch them immediately instead of leaving work queued. If `followup_source_tasks` appear, inspect them with `{task_cmd} action=followups id=<task-id>` and default to `{task_cmd} action=promote_followups id=<task-id>` unless you have an explicit reason to waive specific followups.\n\
12) If there is no task or worker action required, reply with one short status line and stop.\n\
13) Do NOT send readiness acknowledgements to the director or user.\n\
14) Do NOT use built-in messaging tools like `SendMessage`; only use Brehon MCP tools.\n\
15) Do not narrate MCP bootstrap/tool calls in normal text. Keep startup output to one short status line at most."
        ),
        project_policy,
    )
}

pub fn build_reviewer_startup_prompt(
    name: &str,
    agent_cmd: &str,
    verification_cmd: &str,
    project_policy: Option<&str>,
) -> String {
    append_project_policy(
        format!(
            "Brehon reviewer startup. You are reviewer '{name}' (agent_type=reviewer).\n\
1) Do NOT proactively discover, reconnect, or call Brehon MCP tools during idle startup. Stay idle until you receive a review request prompt or an explicit review-obligation nudge.\n\
2) When real review work arrives, use the Brehon MCP verification tool directly from that turn. Do not preflight with {agent_cmd} action=session_start or {agent_cmd} action=whoami unless the prompt explicitly requires it.\n\
3) Do not do supervisor planning work. Only the supervisor brainstorms, creates epics, or decomposes tasks.\n\
4) Your checkout root is the current worktree directory. Treat every file path in review prompts, findings, and task titles as repository-relative to that root unless the prompt explicitly says otherwise.\n\
5) When a review request arrives, do not edit files. Review the task-scoped diff and supplied context only.\n\
6) Evaluate the code across these dimensions: correctness, security, performance, concurrency, error handling, maintainability.\n\
7) Be strict about review debt. Do not waive, dismiss, or summarize away legitimate nitpicks; submit each real nitpick as a structured finding with severity `nitpick`.\n\
8) Only omit a nitpick when it is demonstrably false, duplicate, or outside the requested diff, and mention that reasoning in the summary instead of silently ignoring it.\n\
9) Treat missing or insufficient tests as a real review gap unless the task or project policy explicitly waived tests.\n\
10) Score 1-10: 1-3=reject, 4-5=blocking changes, 6=real uncertainty or insufficient verification, 7=acceptable with all non-blocking issues captured, 8-9=good with only minor captured issues, 10=clean with no findings.\n\
11) Submit your review by calling the Brehon MCP tool `{verification_cmd}` directly. Do NOT use shell/Bash to run `{verification_cmd}` as a command string.\n\
12) Required submit_review arguments: `action=submit_review`, `review_id=<review_id_from_prompt>`, `reviewer={name}`, `score=<1-10>`, `verdict=<approved|needs_revision|rejected>`, and `findings=[{{\"description\":\"...\",\"file\":\"...\",\"line\":42,\"severity\":\"blocking|suggestion|nitpick\",\"suggestion\":\"...\"}}]`.\n\
13) IMPORTANT: Use the review_id from the review request prompt, not the task_id. Always include reviewer={name}.\n\
14) Do not send a normal agent message instead of the structured review result.\n\
15) After submitting the review, stop and wait for the next request.\n\
16) Do not narrate MCP bootstrap/tool calls in normal text. Before any review request arrives, emit at most one short readiness line.\n\
17) If Brehon MCP tools are temporarily unavailable when real review work arrives, stop and wait for a fresh turn instead of polling, sleeping, or running shell commands to check MCP availability."
        ),
        project_policy,
    )
}

pub fn build_advisor_startup_prompt(
    name: &str,
    agent_cmd: &str,
    advisor_cmd: &str,
    project_policy: Option<&str>,
) -> String {
    append_project_policy(
        format!(
            "Brehon advisor startup. You are advisor '{name}' (agent_type=advisor).\n\
1) On startup, register once: {agent_cmd} action=session_start name={name} agent_type=advisor.\n\
2) Your job is brainstorming, critique, comparison, and synthesis inside advisor rooms.\n\
3) Use `{advisor_cmd}` for advisor room state. Read with `action=read`; answer with `action=post`.\n\
4) Stay read-only. Do not create, assign, close, merge, review, or take ownership of tasks.\n\
5) Do not edit repository files, run build/test commands, or mutate runtime state unless a future prompt explicitly grants that permission.\n\
6) Keep replies timely. If you need more context, ask one concrete question or state the uncertainty and stop.\n\
7) Never poll, sleep, or keep a turn open waiting for other advisors. Post your contribution and end the turn.\n\
8) When a room uses debate mode, surface real disagreement first, then offer a compact recommendation if one is clear.\n\
9) If asked for a synthesis, name the decision, the reasons, and the residual risks. Keep it short enough to scan in the TUI.\n\
10) Do not send readiness acknowledgements to the supervisor or user."
        ),
        project_policy,
    )
}

pub fn build_research_startup_prompt(
    name: &str,
    agent_cmd: &str,
    research_cmd: &str,
    pool_id: Option<&str>,
    project_policy: Option<&str>,
) -> String {
    let pool_hint = pool_id
        .map(str::trim)
        .filter(|pool| !pool.is_empty())
        .map(|pool| format!(" pool={pool}"))
        .unwrap_or_default();

    append_project_policy(
        format!(
            "Brehon research startup. You are research agent '{name}' (agent_type=research).\n\
1) On startup, register once: {agent_cmd} action=session_start name={name} agent_type=research.\n\
2) Claim one job with `{research_cmd} action=claim_next{pool_hint}`. Work on only that claimed job until you submit it.\n\
3) Research jobs are read-only. Do not edit repository files, task status, review state, or runtime control-plane files except through `{research_cmd} action=submit`.\n\
4) Produce structured, cited context. Prefer concise summaries, concrete file/spec references, and uncertainty notes over broad prose.\n\
5) Submit completed work with `{research_cmd} action=submit task_id=<task_id> job_id=<job_id> summary=\"...\" content=\"...\" citations='[...]'`.\n\
6) If a job cannot be answered from available context, submit a brief artifact explaining the gap instead of blocking a worker.\n\
7) After every successful submit, immediately call `{research_cmd} action=claim_next{pool_hint}` again and continue one job at a time until it returns idle/no queued work.\n\
8) Stop only when no queued job is available for your pool. Do not poll, sleep, or keep a turn open waiting for future jobs.\n\
9) Do not send readiness acknowledgements to the supervisor or user."
        ),
        project_policy,
    )
}

pub fn build_native_agent_system_prompt(
    role: &str,
    name: &str,
    agent_type: &str,
    cwd: &str,
    tool_prefix: &str,
    supervisor_name: &str,
    project_policy: Option<&str>,
) -> String {
    let agent_cmd = format!("{tool_prefix}agent");
    let advisor_cmd = format!("{tool_prefix}advisor");
    let research_cmd = format!("{tool_prefix}research");
    let task_cmd = format!("{tool_prefix}task");
    let verification_cmd = format!("{tool_prefix}verification");
    let role_contract = match role.trim().to_ascii_lowercase().as_str() {
        "supervisor" => {
            build_supervisor_startup_prompt(name, &agent_cmd, &task_cmd, project_policy)
        }
        "reviewer" => {
            build_reviewer_startup_prompt(name, &agent_cmd, &verification_cmd, project_policy)
        }
        "advisor" => build_advisor_startup_prompt(name, &agent_cmd, &advisor_cmd, project_policy),
        "research" => build_research_startup_prompt(
            name,
            &agent_cmd,
            &research_cmd,
            Some(agent_type),
            project_policy,
        ),
        "worker" | "" => build_worker_startup_prompt(
            name,
            supervisor_name,
            &agent_cmd,
            &task_cmd,
            project_policy,
        ),
        _ => append_project_policy(
            format!(
                "Brehon agent startup. You are agent '{name}' with role '{role}'. Use `{agent_cmd}` for coordination and stay within the current worktree."
            ),
            project_policy,
        ),
    };

    format!(
        "You are Brehon's native Rust agent runtime for {role} '{name}' ({agent_type}) operating in '{cwd}'.\n\
This is not a thin provider chat loop: tool use, permissions, cancellation, task state, and Brehon coordination are runtime-owned behavior.\n\
Use read_file, search_text, list_files, write_file, replace_in_file, and bash for repository work. Shell and edit tools are mediated by Brehon permissions and safety checks; if a command is denied, treat the denial as authoritative and continue or mark the work blocked instead of waiting for a human approval loop.\n\
Use {tool_prefix}* tools for Brehon coordination, task context, skills, rules, memories, factory actions, and review submission. Do not invent tool results, task state, commits, tests, or file edits.\n\
Stay inside the current worktree. Do not claim commands or edits happened unless tool output confirms them.\n\n\
Brehon role contract:\n{role_contract}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_prompt_uses_shared_task_lifecycle() {
        let prompt = build_worker_startup_prompt(
            "worker-1",
            "supervisor",
            "mcp_brehon_agent",
            "mcp_brehon_task",
            None,
        );

        assert!(prompt.contains("Brehon worker startup"));
        assert!(prompt.contains("mcp_brehon_task action=progress"));
        assert!(prompt.contains("mcp_brehon_task action=complete"));
        assert!(prompt.contains("Do NOT proactively call"));
        assert!(prompt.contains("The supervisor owns reviewer assignment"));
    }

    #[test]
    fn reviewer_prompt_requires_structured_review_submission() {
        let prompt = build_reviewer_startup_prompt(
            "reviewer-1",
            "mcp_brehon_agent",
            "mcp_brehon_verification",
            Some("Review for correctness."),
        );

        assert!(prompt.contains("Brehon reviewer startup"));
        assert!(prompt.contains("mcp_brehon_verification"));
        assert!(prompt.contains("action=submit_review"));
        assert!(prompt.contains("reviewer=reviewer-1"));
        assert!(prompt.contains("Do not waive, dismiss, or summarize away legitimate nitpicks"));
        assert!(prompt.contains("10=clean with no findings"));
        assert!(prompt.contains("Project policy:\nReview for correctness."));
    }

    #[test]
    fn native_system_prompt_embeds_role_contract() {
        let prompt = build_native_agent_system_prompt(
            "worker",
            "worker-1",
            "native-agent",
            "/repo",
            "mcp_brehon_",
            "supervisor",
            None,
        );

        assert!(prompt.contains("native Rust agent runtime"));
        assert!(prompt.contains("runtime-owned behavior"));
        assert!(prompt.contains("Brehon worker startup"));
        assert!(prompt.contains("mcp_brehon_task action=complete"));
    }

    #[test]
    fn advisor_prompt_is_read_only_and_room_scoped() {
        let prompt = build_advisor_startup_prompt(
            "advisor-1",
            "mcp_brehon_agent",
            "mcp_brehon_advisor",
            Some("Prefer concise synthesis."),
        );

        assert!(prompt.contains("Brehon advisor startup"));
        assert!(prompt.contains("read-only"));
        assert!(prompt.contains("action=post"));
        assert!(prompt.contains("Never poll"));
        assert!(prompt.contains("Project policy:\nPrefer concise synthesis."));
    }

    #[test]
    fn research_prompt_claims_pool_and_stays_read_only() {
        let prompt = build_research_startup_prompt(
            "research-1",
            "mcp_brehon_agent",
            "mcp_brehon_research",
            Some("specs"),
            Some("Cite primary sources."),
        );

        assert!(prompt.contains("Brehon research startup"));
        assert!(prompt
            .contains("mcp_brehon_agent action=session_start name=research-1 agent_type=research"));
        assert!(prompt.contains("mcp_brehon_research action=claim_next pool=specs"));
        assert!(prompt.contains("After every successful submit"));
        assert!(prompt.contains("read-only"));
        assert!(prompt.contains("Project policy:\nCite primary sources."));
    }
}

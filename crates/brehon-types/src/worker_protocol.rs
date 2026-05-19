/// How the worker protocol should describe bootstrap expectations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerBootstrapMode {
    /// The worker is starting from an idle pane and should not proactively
    /// register or poll unless actual work arrives.
    IdleStartup,
    /// The worker has already called `session_start` and should continue from
    /// the registered session without repeating bootstrap calls.
    SessionRegistered,
}

/// Render the shared worker protocol used across startup prompts and
/// `session_start` instructions.
pub fn build_worker_protocol(
    bootstrap: WorkerBootstrapMode,
    agent_cmd: &str,
    task_cmd: &str,
    supervisor_name: &str,
) -> String {
    let bootstrap_step = match bootstrap {
        WorkerBootstrapMode::IdleStartup => format!(
            "Do NOT proactively call `{agent_cmd} action=session_start` or \
             `{agent_cmd} action=whoami` during idle startup. Brehon already \
             tracks this pane for worker availability."
        ),
        WorkerBootstrapMode::SessionRegistered => format!(
            "`{agent_cmd} action=session_start` already registered this worker. \
             Do not repeat bootstrap calls or poll with `{agent_cmd} action=whoami` \
             unless you have lost task context."
        ),
    };

    let steps = vec![
        // ── Worktree containment (rules 1-3) ──────────────────────────────
        // These run first because they're the most-violated and most-damaging
        // when ignored. A worker that drifts off the worker branch or out of
        // the worktree produces empty commits and stranded changes — the
        // damage is silent and only surfaces at review time.
        format!(
            "WORKTREE RULE: Work from the current worktree directory and stay on your dedicated \
             worker branch. NEVER run `git checkout`, `git switch`, `git reset --hard`, or \
             `git restore --source=...` against `main`, `master`, `develop`, `trunk`, or the \
             task's `merge_target` branch from this pane. If you find yourself on any of those \
             branches, that is a bug — call `{task_cmd} action=update id=<task> status=blocked` \
             and message the supervisor instead of proceeding."
        ),
        "WORKTREE RULE: Never `cd` to the shared repo root, `BREHON_PROJECT_ROOT`, or any path \
         outside the current worktree. Do not use absolute filesystem paths outside the current \
         worktree even for comparison, testing, or read-only inspection unless the supervisor \
         explicitly tells you to stop and wait for manual intervention."
            .to_string(),
        "WORKTREE RULE: If a shell/tool command is denied because it would access an external \
         directory or requires approval, treat that denial as a hard constraint. Do NOT wait \
         for approval, do NOT retry the same command, and do NOT keep probing other \
         outside-worktree paths. Continue using only the current worktree, or mark the task \
         blocked and message the supervisor with the denied command and why you thought it \
         was needed."
            .to_string(),
        // ── Bootstrap and task lifecycle ──────────────────────────────────
        bootstrap_step,
        format!(
            "If this pane already shows an assigned task, or a real assignment prompt arrives, \
             call `{task_cmd} action=mine` at most once from that turn to recover task details. \
             If no task is currently assigned, send one ready message to `{agent_cmd} action=message \
             target={supervisor_name}` and stop. Do not poll or loop on `{task_cmd} action=mine`."
        ),
        format!(
            "If `{task_cmd} action=mine` reports no tasks, or reports only tasks in \
             `review_ready`, `in_review`, `approved`, `blocked`, `merged`, or `closed`, stop and \
             wait. Do not resume implementation while review, approval, integration, or a blocked \
             state is active."
        ),
        format!(
            "Use Brehon MCP tools for task/state coordination, and normal shell/CLI commands \
             for repo work — always within the current worktree (see worktree rules above)."
        ),
        "Do not do supervisor planning work. Do not brainstorm implementation plans, create epics, or decompose tasks. Only the supervisor does that.".to_string(),
        format!(
            "Report progress early and whenever the work phase changes: `{task_cmd} action=progress \
             id=<task> percent=<n> notes=\"<summary>\" activity=<reading|editing|testing|reviewing>`. \
             This is what moves the task from `assigned` to `in_progress`; shell output or git commits \
             alone do not change Brehon task status."
        ),
        format!(
            "If you need a safe mid-task snapshot without finishing, call `{task_cmd} action=checkpoint \
             id=<task> message=\"<summary>\"`."
        ),
        format!(
            "When implementation is complete, call `{task_cmd} action=complete id=<task> \
             notes=\"<summary>\" activity=testing`. You may add `message=\"<checkpoint summary>\"` \
             if the checkpoint commit message should differ. `complete` creates or records the \
             checkpoint commit, moves the task to `review_ready`, and notifies the supervisor \
             in one call."
        ),
        "After `task action=complete`, stop. Do not monitor review progress, do not call \
         `verification action=review_status`, and do not call `verification action=request_review`. \
         The supervisor owns reviewer assignment, review polling, and any follow-up reassignment."
            .to_string(),
        format!(
            "If blocked, call `{task_cmd} action=update id=<task> status=blocked`, then message the \
             supervisor via `{agent_cmd} action=message target={supervisor_name}`."
        ),
        "Keep executing until blocked or complete. Do not stop at status-only replies.".to_string(),
        format!(
            "Do not narrate MCP bootstrap or tool calls in normal text. After startup, emit at most \
             one short readiness line unless you have real task work. Do NOT message target=brehon. \
             Use target={supervisor_name}."
        ),
        format!(
            "Never call `{task_cmd} action=close`. You cannot merge tasks. Only the supervisor can \
             close or merge them."
        ),
        format!(
            "If review requests changes, fix the issues and call `{task_cmd} action=complete` again \
             when the new revision is ready."
        ),
        format!(
            "NEVER claim completion in a progress note. Prose like \"task is now in review\", \
             \"ready for review\", or \"task is complete\" in `{task_cmd} action=progress` does NOT \
             change the task's status — `progress` only updates the progress field. The MCP will \
             reject such claims. The ONLY way to move a task to review is \
             `{task_cmd} action=complete`. If you believe your work is done, call complete. If \
             you are still working, use neutral language (\"tests passing\", \"investigating\", \
             \"75% through the fix list\") and report percent<100 until the fix is truly ready."
        ),
        // ── Closing reminder ─────────────────────────────────────────────
        // Repeated at the end of the protocol because models with weaker
        // long-range instruction following (the reason this protocol exists
        // at all) tend to honor whatever they read most recently. The earlier
        // worktree rules state the same thing; this is intentional repetition.
        "REMINDER: every command you run executes in your dedicated worktree. Never `cd` out of \
         it. Never `git checkout main` (or master / develop / trunk / the task's merge_target). \
         If you do work on the wrong branch, your commit will be empty and your progress will \
         silently disappear. When in doubt, run `git rev-parse --abbrev-ref HEAD` and confirm \
         you are on your worker branch before editing."
            .to_string(),
    ];

    steps
        .iter()
        .enumerate()
        .map(|(idx, step)| format!("{}) {step}", idx + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::{build_worker_protocol, WorkerBootstrapMode};

    #[test]
    fn worker_protocol_forbids_review_orchestration_after_complete() {
        let protocol = build_worker_protocol(
            WorkerBootstrapMode::IdleStartup,
            "agent",
            "task",
            "supervisor",
        );

        assert!(protocol.contains("task action=complete"));
        assert!(protocol.contains("After `task action=complete`, stop."));
        assert!(protocol.contains("do not call `verification action=review_status`"));
        assert!(protocol.contains("do not call `verification action=request_review`"));
        assert!(protocol.contains("The supervisor owns reviewer assignment"));
        assert!(protocol.contains("Never `cd` to the shared repo root"));
        assert!(protocol.contains("Do NOT wait for approval"));
    }

    #[test]
    fn worktree_rules_appear_first_and_repeat_at_end() {
        let protocol = build_worker_protocol(
            WorkerBootstrapMode::IdleStartup,
            "agent",
            "task",
            "supervisor",
        );

        // Worktree containment must be the first thing the model reads —
        // ordering matters because weaker instruction-followers anchor on
        // the opening of the prompt.
        let first_line = protocol.lines().next().unwrap_or("");
        assert!(
            first_line.contains("WORKTREE RULE"),
            "first protocol step must be a worktree rule, got: {first_line}"
        );

        // And repeat at the end so recency-biased models see it again.
        let last_line = protocol.lines().last().unwrap_or("");
        assert!(
            last_line.contains("REMINDER")
                && last_line.contains("worker branch"),
            "last protocol step must repeat the worktree reminder, got: {last_line}"
        );

        // Explicit list of forbidden checkout targets so the model can't
        // wriggle out via "main is not main, it's origin/main".
        assert!(protocol.contains("`main`, `master`, `develop`, `trunk`"));
        assert!(protocol.contains("`merge_target`"));
        assert!(protocol.contains("git reset --hard"));
    }
}

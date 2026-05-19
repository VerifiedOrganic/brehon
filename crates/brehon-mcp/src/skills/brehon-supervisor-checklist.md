---
name: brehon-supervisor-checklist
description: Brehon supervisor checklist. Use at session start, after restart, before closeout, and when the supervisor needs a concise recovery checklist.
roles:
  - supervisor
tags:
  - supervisor
  - checklist
  - recovery
---

# Brehon Supervisor Checklist

Use this as a quick audit, not as a polling loop.

## Session Start Or Restart

1. `mcp__brehon__agent action=whoami`
2. `mcp__brehon__search_skills query=""`
3. `mcp__brehon__search_rules query=""`
4. `mcp__brehon__search_memories query="recent decisions" limit=5`
5. `mcp__brehon__task action=list task_type=initiative`
6. `mcp__brehon__task action=list task_type=epic`
7. `mcp__brehon__task action=conflicts`
8. `mcp__brehon__task action=ready`
9. `mcp__brehon__task action=list status=review_ready`
10. `mcp__brehon__task action=list status=blocked`

Call `mcp__brehon__factory action=worker_status` only when assignment, recovery, or stall handling needs worker inventory.
Respect `assignment_mode=reserved`: the `codex-hardening` worker lane is for final hardening tasks with matching `execution_policy`, not normal queue work.

## Recovery Checks

- Orphaned `in_progress` or `changes_requested` task: reassign with `factory action=assign_workers`.
- `review_ready` task: request review with `verification action=request_review`.
- Stuck review: inspect `verification action=review_status`, then `verification action=reassign_panel` if reviewers are dead.
- Approved merge task: integrate or close before dispatching new work.
- Open followups: inspect with `task action=followups`, then promote or explicitly waive.
- Supervisor-owned integration conflict: resolve it before normal dispatch.

## Before Marking Work Complete

- `mcp__brehon__task action=children id=<initiative-or-epic-id>`
- every worker task is `merged` or `closed`
- for initiatives, run `mcp__brehon__task action=ensure_final_hardening id=<initiative-id>` if the `Final Hardening and Cross-Epic Cleanup` epic is missing, then ensure it is closed
- every completed task has an approved review; supervisor approval override is not allowed
- open followups are promoted or waived by id with reasons
- required project tests passed
- close the epic or initiative with `mcp__brehon__task action=close id=<id>`

---
name: brehon-dispatch
description: Brehon supervisor dispatch workflow. Use when tasks exist and the supervisor needs to assign workers, process review-ready work, handle blockers, integrate approved tasks, or close epics.
roles:
  - supervisor
tags:
  - supervisor
  - execution
  - coordination
---

# Brehon Dispatch

Use this once the hierarchy exists and work needs to move.

## Goal

Keep the frontier moving without losing review, integration, or followup obligations.

## State Read

Start with:

- `mcp__brehon__task action=conflicts`
- `mcp__brehon__task action=ready`
- `mcp__brehon__task action=list status=in_progress`
- `mcp__brehon__task action=list status=blocked`
- `mcp__brehon__factory action=worker_status`

`task action=ready` is the main dispatch queue. It can include conflicts, ready worker tasks, review-ready tasks, changes-requested tasks, approved tasks, stalled tasks, and followup source tasks.

## Queue Order

Process in this order:

1. `integration_conflict_tasks`: resolve or explicitly triage supervisor-owned conflicts first.
2. `review_ready_tasks`: call `mcp__brehon__verification action=request_review task_id=<task-id>`.
3. `approved_tasks`: integrate or close before starting new work.
4. `changes_requested_tasks`: reassign to a worker; stored `review_feedback` carries prior blockers.
5. `stalled_tasks`: inspect delivery and worker status before re-nudging. Reassign if the worker acknowledged but did not act.
6. `followup_source_tasks`: inspect and promote, or waive by explicit id and reason.
7. `tasks`: assign pending worker tasks to idle workers.

After any queue-changing action, call `mcp__brehon__task action=ready` again before ending your turn.

## Assignment

- Spawn only if needed: `mcp__brehon__factory action=spawn_workers count=N`.
- Assign with one operation: `mcp__brehon__factory action=assign_workers task_id=<task-id> workers=<worker-name>`.
- Respect `execution_policy` from the task and the `assignment_mode` from `worker_status`. Reserved workers, such as `codex-hardening`, are only for tasks whose policy explicitly targets their lane and accepted work class.
- Do not separately message initial assignment context unless the task record lacks necessary detail.
- Do not assign a new task to a worker whose current task is `review_ready`, `in_review`, `approved`, or otherwise non-terminal.
- Do not archive `review_ready`, `in_review`, `approved`, `changes_requested`, or checkpointed tasks to move the graph forward. Broken reviewer capacity is an operator stop, not a review bypass.
- In shared-directory mode, set ownership before parallel edits:
  - `mcp__brehon__factory action=set_ownership task_id=<task-id> worker=<worker-name>`

## Reviews

When a worker completes a task, it should be `review_ready`.

1. Request review:
   - `mcp__brehon__verification action=request_review task_id=<task-id>`
2. Check progress only when needed:
   - `mcp__brehon__verification action=review_status task_id=<task-id>`
3. Reassign dead panel members:
   - `mcp__brehon__verification action=reassign_panel task_id=<task-id>`
4. If approved, close or integrate based on completion mode and merge target.
5. If changes are requested, reassign the task to a worker and let the stored `review_feedback` drive the fix.

Never use `task action=update status=in_review`; it does not seat a panel.

## Closeout

Before closing an epic or initiative:

- `mcp__brehon__task action=children id=<id>`
- verify all subtasks reached terminal state
- run required project tests when the closeout changes code integration state
- close the epic or initiative with `mcp__brehon__task action=close id=<id>`

## Exit

After assignments, unblock messages, review requests, or closeout actions, stop once `task action=ready` shows no immediate supervisor action. Do not poll just to watch workers.

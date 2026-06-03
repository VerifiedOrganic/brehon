---
name: brehon-breakdown
description: Brehon supervisor breakdown workflow. Use after design approval to decompose an initiative or epic into executable epics, worker tasks, dependencies, file hints, and test requirements.
roles:
  - supervisor
tags:
  - supervisor
  - planning
  - decomposition
---

# Brehon Breakdown

Use this after `brehon-discovery` has produced an approved design and top-level container.

## Goal

Create an execution graph that workers can execute without rediscovering the plan.

## Rules

- Do not dispatch workers from this skill.
- Every implementation task must be a child of an epic.
- Use one initiative only for genuinely multi-phase work.
- Keep dependencies real. Do not serialize independent work.
- Use `completion_mode=close` for audit, research, design, docs, and no-code tasks.
- Do not create worker tasks for live `.brehon` control-plane state.

## Load The Container

- `mcp__brehon__task action=list task_type=initiative`
- `mcp__brehon__task action=list task_type=epic`
- `mcp__brehon__task action=children id=<initiative-or-epic-id>`

If the container has no approved design or acceptance criteria, stop and return to `brehon-discovery`.

## Build The Hierarchy

For initiatives, create phase epics first:

- `mcp__brehon__task action=create task_type=epic parent_id=<initiative-id> title="..." description="..." acceptance_criteria=["..."] plan_steps=["..."] implementation_notes="..."`

Then ensure one tail epic:

- `mcp__brehon__task action=ensure_final_hardening id=<initiative-id>`

This creates or backfills `Final Hardening and Cross-Epic Cleanup`, dependencies on the phase epic ids, and these seeded tasks:

- `Final hardening triage` with `completion_mode=close`
- `Resolve deferred cross-epic seams`
- `Final validation and operator readiness pass`

Seeded final hardening tasks carry `execution_policy.work_class=final_hardening`, `preferred_lane=codex-hardening`, `preferred_model=gpt-5.5`, and `preferred_reasoning_effort=xhigh`.

For each epic, create worker tasks:

- `mcp__brehon__task action=create parent_id=<epic-id> title="..." description="..." acceptance_criteria=["..."] file_hints=["..."] test_requirements=["..."] plan_steps=["..."] implementation_notes="..." priority=high`

Use top-level structured fields. If forced into `description`, include canonical headings: `Acceptance Criteria:`, `File Hints:`, `Test Requirements:`, `Plan:`, `Implementation Notes:`.

## Task Quality Bar

Each worker task needs:

- one observable outcome
- concrete acceptance criteria
- file/module hints when known
- explicit test or verification requirements
- short execution notes for risky constraints
- dependencies only when another task must finish first
- enough context for a weaker worker model to execute without broad repo search

Prefer 3-8 worker tasks per epic. Split tasks that cross unrelated ownership areas. Merge tasks that cannot be verified independently.

## Validate

Run:

- `mcp__brehon__task action=children id=<initiative-or-epic-id>`
- `mcp__brehon__task action=ready`

Check for:

- orphan tasks not under the intended epic
- vague titles
- missing acceptance criteria
- missing file hints for implementation tasks
- fake dependencies
- tasks too broad for one worker

## Exit

When the graph is clean and ready tasks exist, switch to `brehon-dispatch`.

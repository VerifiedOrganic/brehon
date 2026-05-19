---
name: brehon-discovery
description: Brehon supervisor discovery workflow. Use for unclear user requests, planning conversations, design tradeoffs, and deciding whether work should become an epic or an initiative.
roles:
  - supervisor
tags:
  - supervisor
  - planning
  - brainstorming
  - discovery
---

# Brehon Discovery

Use this before task creation when the user request is not yet a concrete execution plan.

## Goal

Produce an approved design and top-level Brehon container. Do not create worker subtasks until the design is approved.

## Intake

Load only the context needed to avoid duplicate or conflicting plans:

- `mcp__brehon__search_memories query="<topic>" limit=5`
- `mcp__brehon__task action=list task_type=initiative`
- `mcp__brehon__task action=list task_type=epic`
- `mcp__brehon__task action=list status=in_progress`

Clarify:

- problem and user outcome
- in-scope and out-of-scope
- technical constraints
- acceptance criteria
- risks, unknowns, and dependencies
- whether the work is one feature or a multi-phase program

Ask one focused question at a time when the request is fuzzy. If the user asks you to proceed, make conservative assumptions and state them.

## Design Output

Present a compact design with:

- recommended approach first
- rejected alternatives and why
- affected components
- data/control flow
- failure modes and recovery
- test strategy
- rollout or migration notes when relevant

Get explicit user approval before decomposition unless the user has already authorized implementation.

## Record And Containerize

Record approved designs:

- `mcp__brehon__create_memory content="Design for <feature>: ..." tags=["design", "approved"]`

Then create exactly one top-level container:

- One bounded feature:
  - `mcp__brehon__task action=create task_type=epic title="..." description="..." acceptance_criteria=["..."] plan_steps=["..."] implementation_notes="..."`
- Multi-phase work:
  - `mcp__brehon__task action=create task_type=initiative title="..." description="..." acceptance_criteria=["..."] plan_steps=["Phase 1", "Phase 2"] implementation_notes="..."`

Prefer structured top-level fields. If structure must go into `description`, use these headings so Brehon can recover it: `Acceptance Criteria:`, `Plan:`, `Implementation Notes:`.

## Exit

When the approved container exists, switch to `brehon-breakdown`.

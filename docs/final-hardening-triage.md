# Final hardening triage

This report records the final hardening triage snapshot for initiative
`initiative/brehon-residual-runtime-hardening-after-permission-sandbox-reset-da434943`
as of May 26, 2026. It audits the seeded final hardening epic
`T-f888dfdb`, the four closed phase epics that feed it, the current
operator docs and validation harness, and the late-arriving research
artifacts `RCH-T-c696606a-product-context-001` and
`RCH-T-c696606a-code-map-001`. The goal is to show which deferred
cleanup candidates already became concrete tasks, which compatibility
surfaces remain intentional, and which remaining seams still need explicit
final-hardening ownership.

## Final hardening epic snapshot

This section captures the current task surface that final hardening owns.
The final hardening epic currently contains only the three seeded tasks:
`T-c696606a` (triage), `T-a28893bf` (cross-epic seams), and `T-a022c23d`
(final validation and operator readiness). No extra supervisor-added tasks,
and no approved-review follow-up items, are attached to `T-f888dfdb` at
this snapshot.

- **Evidence:** `task action=children id=T-f888dfdb` returned only the three
  seeded tasks.
- **Evidence:** `task action=followups id=T-f888dfdb include_resolved_followups=true`
  returned zero follow-up items.
- **Evidence:** The current `crates/brehon-gatekeeper/src/` tree contains
  only `layers/go_build.rs` and `layers/mod.rs`. There is no crate root or
  `findings.rs` module in this worktree snapshot, so the gatekeeper findings
  surface is a pre-existing partial stub rather than an auditable finding
  source for this task.
- **Disposition:** There are no open gatekeeper findings or supervisor-added
  cleanup items to deduplicate right now. The gatekeeper gap itself is
  pre-existing and out of scope for this final-hardening triage.

## Phase 1 cleanup candidates

This section covers reviewer delivery, reviewer recovery, and assignment
propagation cleanup that could have leaked into the final hardening epic.
The work was already pulled forward into concrete phase tasks instead of
being deferred.

- **Evidence:** Phase 1 closed concrete tasks `T-376f17c1`, `T-46a437a6`,
  and `T-77f09766`.
- **Evidence:** Phase 1 also closed follow-up tasks `T-6058884b`,
  `T-362e9bc5`, `T-e0737fc0`, `T-1807066e`, `T-b6ffebaf`, and
  `T-0e5194ea`.
- **Disposition:** Reviewer delivery and review recovery drift was already
  converted into concrete work and closed inside phase epic `T-5b07e4ef`.
  No separate triage task is needed for these already-landed items.

## Phase 2 cleanup candidates

This section covers unattended approval behavior, prompt-blocked runtime
visibility, and gate-test cleanup that could have become vague hardening
scope later. Phase 2 already resolved those items as named tasks.

- **Evidence:** Phase 2 closed concrete tasks `T-71bb5239`, `T-1b52c7bc`,
  `T-f7c78899`, and `T-6fae0903`.
- **Evidence:** Phase 2 also closed follow-up tasks `T-7a7ebd3d`,
  `T-df77c9df`, and `T-65269b72`.
- **Disposition:** Unattended approval and runtime prompt cleanup was already
  converted into concrete work and closed inside phase epic `T-14f2b93b`.
  No separate triage task is needed for these already-landed items.

## Phase 3 cleanup candidates

This section covers duplicate-import refusal, dirty-root close guards, and
stale worktree maintenance. These are the main cross-epic hygiene seams that
could have required a late cleanup pass, but each one already became a named
phase task.

- **Evidence:** Phase 3 closed concrete tasks `T-639ab0da`, `T-c493c646`,
  and `T-983847db`.
- **Evidence:** Phase 3 also closed follow-up tasks `T-31981e45`,
  `T-656b7d09`, `T-586a7775`, and `T-a7ed7ea0`.
- **Disposition:** Git, worktree, and import hygiene cleanup was already
  converted into concrete work and closed inside phase epic `T-3e6fcae2`.
  No separate triage task is needed for these already-landed items.

## Phase 4 cleanup candidates

This section covers cross-agent compatibility, shutdown contract alignment,
small DRY cleanups, and soak-related finishing work. The initiative already
turned these into concrete tasks before the phase landed.

- **Evidence:** Phase 4 closed concrete tasks `T-eeb9cd19`, `T-eef64d1a`,
  `T-34b8e73e`, `T-8d46b4dd`, and `T-68a6db50`.
- **Evidence:** Phase 4 also closed follow-up tasks `T-46a29111` and
  `T-0234aea7`.
- **Disposition:** Cross-agent seams and cleanup drift were already
  converted into concrete work and closed inside phase epic `T-74c01cb1`.
  No separate triage task is needed for these already-landed items.

## Research artifact reconciliation

This section reconciles the late-arriving research artifacts against the
current branch so final hardening does not reopen work that already landed.
The code-map artifact confirmed that the final-hardening scaffolding and
gatekeeper-facing task surfaces did not add new cleanup items. The
product/spec artifact raised a broader set of historical candidates, so this
section records only the subset that I re-verified on the current branch plus
the one staged seam that still remains live.

- **Rechecked on the current branch (`0fc71e06e2f113eceb609bf5e182f9add3029cd3`):**
  - `INJECTED_KILL_FAILURE` is present at
    `crates/brehon-orchestrator/src/test_support.rs:14`, and
    `crates/brehon-orchestrator/src/orchestrator.rs:583`,
    `crates/brehon-orchestrator/src/orchestrator.rs:1473`, and
    `crates/brehon-orchestrator/src/orchestrator.rs:1532` import and assert
    on that shared constant.
  - `Orchestrator::shutdown()` is present at
    `crates/brehon-orchestrator/src/orchestrator.rs:468`.
  - `WorkerPool::shutdown()` is present as a test-only helper at
    `crates/brehon-orchestrator/src/worker_pool.rs:549-550`, so the
    product/spec artifact's shutdown concern does not remain as an open
    production seam on this commit.
  - Prompt-injection cleanup is present at
    `crates/brehon-mux/src/agent_config.rs:97-105`,
    `crates/brehon-adapter-sdk/src/harness.rs:569-590`, and
    `crates/brehon-mux/src/pane/tests/injection.rs:243-244` plus
    `crates/brehon-mux/src/pane/tests/injection.rs:299-302`, covering the
    invalid-strategy warning, round-trip parsing tests, and Agy assertions.
  - Review-obligation failure tracking is pruned in
    `crates/brehon-tui/src/run/stall_handling.rs:552-562` and the prune call
    is wired into the live stall-handling path at
    `crates/brehon-tui/src/run/stall_handling.rs:1812-1815`.
- **Concrete remaining candidate:**
  - `crates/brehon-tui/src/run/recovery.rs` explicitly marks
    `attempt_auto_recover_stalled_worker`,
    `inspect_worker_worktree_state`, and
    `escalate_worker_unmerged_conflict` as staged infrastructure that is
    unit-tested but not wired into the production event loop.
  - Repository-wide search shows those helpers are referenced from their
    definitions and from test code in `crates/brehon-tui/src/run/mod.rs`,
    but not from the production event loop.
- **Disposition:** Report one concrete final-hardening seam from this
  artifact: wire the staged worker-recovery path into the production TUI
  loop, or explicitly scope it out with supervisor-approved rationale.

## Explicit waivers and deferrals

This section records the items that still look transitional at a glance but
should not be reopened as new cleanup tasks without new evidence.

- **Waive as new cleanup work:** The legacy
  `BREHON_PLAN_EXTRACT_TIMEOUT_SECS` environment variable remains
  backward-compatible behavior in `README.md` and
  `crates/brehon-cli/src/commands/import_plan/extraction.rs`. This is an
  intentional compatibility surface, not new final hardening debt.
- **Waive as new cleanup work:** The legacy `security.sandbox_profile`
  compatibility surface remains documented in `docs/ARCHITECTURE.md`. This
  is intentional operator compatibility, not a cross-epic seam that needs a
  new task.
- **Defer to an existing seeded task:** Final validation already has an
  explicit home in `T-a022c23d`, and the current harness exists at
  `scripts/phase5_stability_gate.sh`. Triage does not need to create a new
  validation task for this work.

## Result

This section summarizes the triage decision that downstream final hardening
work should use.

- **One concrete final-hardening candidate remains live:** the staged worker
  recovery helpers in `crates/brehon-tui/src/run/recovery.rs` are still not
  wired into the production event loop.
- **No existing deferred cleanup candidate was left vague.** The closed phase
  work already absorbed the concrete phase follow-up items, the late research
  artifact mostly pointed at fixes that are now present on this branch, and
  the remaining compatibility surfaces are explicitly waived or deferred.
- **If queued research artifacts attach new evidence later,** add that
  evidence to this epic as a concrete task instead of reopening a vague
  cleanup queue.

## Next steps

This section captures the intended handoff to the remaining seeded final
hardening tasks.

1. Add the staged worker-recovery wiring gap to `T-a28893bf` or a new
   sibling hardening task if the supervisor wants a narrower unit of work.
2. Use `T-a022c23d` for the final validation and operator-readiness pass,
   including `scripts/phase5_stability_gate.sh`.
3. Keep any new issue tied to explicit evidence such as a task ID, review
   finding, integration conflict, operator request, gatekeeper output, or a
   current-branch code reference.

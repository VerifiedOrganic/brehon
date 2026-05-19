# ADR-0004: Multi-reviewer panel with explicit scoring policy

**Status**: Accepted
**Date**: 2026-04-10
**Deciders**: project founders

---

## Context

When AI agents write code, the question "is this work good enough to merge?"
cannot be answered by the agent that wrote it. The agent that wrote it has
seen only its own context, has every incentive to declare success, and is
the one whose blind spots produced any bug that's in the diff.

The common pattern in 2026 is **LLM-as-judge**: a second model reads the
output of the first and emits a verdict. This is better than nothing, but
it has well-documented failure modes:

- **Sycophancy.** A single LLM judge tends to agree with the work it reviews,
  particularly when the work is presented as already complete.
- **Single-model blind spots.** A second instance of the same model shares
  the same prior knowledge, training data, and failure modes as the writer.
- **Score drift.** Numeric scores from a single LLM judge are noisy and
  rarely calibrated across reviews.
- **No accountability for revisions.** When code goes back for changes, a
  fresh judge has no memory of what was already reviewed. Reviewers can
  contradict each other across rounds.

Brehon's goal is reliable software produced by agents over long sessions.
"Reliable" implies real review gates with real teeth. The reviewer architecture
has to provide:

- Multiple independent judgments per review.
- A scoring policy with explicit thresholds, not a vibes-based verdict.
- Continuity across revision rounds — the same reviewers see the rework.
- Structured findings (location, severity, suggestion) so revisions can be
  surgical.
- Bounded revision rounds before human escalation.
- An audit trail of who scored what when.

## Decision

**Code review is performed by a panel of reviewer agents who independently
score the work against a numeric policy. The same panel is bound to a task
across all revision rounds. A configurable threshold decides approval.**

Concretely, in `crates/brehon-review/`:

- A **panel** is a set of reviewer agents drawn from configured reviewer
  lanes. Panel composition (size, diversity across model providers, role
  mix) is configurable.
- Each reviewer receives the same review prompt and the same diff. They
  do not see each other's scores until after submission.
- Each reviewer submits independently via the MCP `verification.submit_review`
  tool:
  - a **score** from 1 to 10,
  - a **verdict** (`approved`, `changes_requested`, `rejected`),
  - zero or more **findings**, each with a file + line + severity
    (`blocking` / `suggestion` / `nitpick`) + suggested fix.
- The **`ScoreCollector`** accumulates submissions.
- The **`ThresholdEvaluator`** applies a `ReviewPolicy` with four levers
  (in `brehon-types`):
  - `min_average` — panel mean must be at least this.
  - `min_individual` — every reviewer must score at least this.
  - `blocking` — no finding with `blocking` severity above this level.
  - `min_approvals` — at least this many reviewers must verdict `approved`.
- The **`FeedbackConsolidator`** deduplicates findings across reviewers,
  preserves dissent (a finding only one reviewer raised is still surfaced),
  and produces a single consolidated report for the worker.
- **Panel affinity**: the same panel stays bound to the task across all
  revision rounds. New round → distinct `review_id`, but the panel roster
  is preserved.
- A `max_rounds` ceiling caps revisions before the supervisor escalates
  to a human supervisor.
- **`ReviewerCalibration`** (`brehon-review/src/calibration.rs`) tracks
  per-reviewer score statistics over time so the supervisor can spot
  outliers (a reviewer who always scores 10, a reviewer who always
  blocks).

The default policy values (subject to per-project overrides) are
`min_average=7`, `min_individual=6`, `blocking<=5`, `min_approvals=2`,
`max_rounds=3`.

Round state is persisted under `.brehon/runtime/reviews/<review_id>/`.
Events emitted for each transition (`ReviewRequested`,
`ReviewScoreReceived`, `ReviewApproved`, `ReviewChangesRequested`,
`ReviewRejected`) are recorded in the event store so the supervisor and
the audit trail can both observe them.

## Consequences

**Accepted:**

- Reviews are more expensive than a single LLM judge call. A 3-reviewer
  panel costs ~3x the tokens of a single judge for the read phase. We
  consider this a feature, not a cost: the wall-clock and money cost of
  *unreliable* review (a bad merge) is far higher than the cost of
  multiple reviews.
- Reviewer disagreement is a first-class condition the system must handle
  (and does, via `FeedbackConsolidator` and supervisor escalation).
- Configuring panel sizes, lanes, and thresholds is a real responsibility
  for the operator. Bad thresholds either rubber-stamp everything (too
  loose) or never approve anything (too tight).
- Panel affinity creates a longer feedback loop for individual reviewer
  prompt iteration — changes to the reviewer system prompt only take
  effect for new panels, not panels already bound to in-flight tasks.

**In exchange:**

- Real review gates with real teeth. A merge requires explicit numeric
  consensus, not a vibes verdict.
- Cross-model diversity. A panel can mix Claude, Codex, and Gemini
  reviewers so the panel's collective blind spots are smaller than any
  individual model's.
- Continuity across rounds. The same reviewers see the rework, so they
  can verify that previously flagged issues were addressed.
- Structured findings. Workers receive specific, actionable feedback
  (file + line + severity + suggestion), not a paragraph of prose.
- Calibration data over time. We can detect a reviewer that drifts and
  rebalance the panel.
- Auditability. Every score and every finding is in the event store with
  a reviewer identity attached. We can replay any review.

## Alternatives considered

**Single LLM-as-judge.** The industry default. Rejected for the
single-model blind spots and sycophancy reasons in the context. We use
panels precisely so that no single agent can rubber-stamp work.

**Pass/fail verdict only, no numeric scoring.** Considered for
simplicity. Rejected because numeric scoring lets the policy enforce
both "everyone approved" and "everyone scored at least 6", which
catches the case where reviewers approve work they consider borderline.
It also enables calibration over time.

**Fresh panel per round.** Considered. Rejected because the new panel
loses the context of why the prior round was rejected. The same
reviewers re-reviewing the same work is the closest approximation we
have to a human reviewer responding to revisions on a PR.

**Consensus-required (unanimous approval).** Considered. Rejected
because a single bad reviewer (sycophant, outlier, or just confused)
can block legitimate merges. The `min_approvals` lever lets the operator
choose; the default `min_approvals=2` with panel size 3 tolerates one
dissenter.

**Quorum-based without scores (just count approvals).** Considered.
Rejected because scoring catches the "everyone approved but nobody loved
it" case, which often signals real but inarticulable problems. Numeric
scores plus structured findings together carry more signal than counts
alone.

**Human review only.** Considered for the highest-stakes paths. Brehon
supports human escalation as the terminal action when the panel cannot
reach a decision within `max_rounds`, but routing every change through
human review defeats the purpose of autonomous orchestration. The panel
is the first line; humans are the last.

## See also

- `crates/brehon-review/` — all review logic.
- `crates/brehon-mcp/src/tools/verification/` — the MCP-facing tools.
- `crates/brehon-types/src/` — `ReviewPolicy`, `ReviewVerdict`,
  `ReviewScore`, `ReviewFinding`, `CommentSeverity`.
- `crates/brehon-cli/tests/review_flow.rs` — end-to-end test.

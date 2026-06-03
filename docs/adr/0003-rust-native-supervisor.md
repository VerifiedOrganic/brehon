# ADR-0003: A deterministic Rust supervisor reading an event store, not an LLM

**Status**: Accepted
**Date**: 2026-04-08
**Deciders**: project founders

---

## Context

Multi-agent orchestration systems commonly designate one agent as the
"supervisor" or "orchestrator" — typically a large language model that
receives status updates from worker agents, decides what to do next, and
issues commands. This pattern is intuitive but expensive in three ways:

1. **Token cost.** Every observation (heartbeat, idle period, partial output)
   either burns tokens or is dropped. At the cadence required to actually
   notice stuck workers, this cost dominates the bill of a long-running
   session.
2. **Latency.** Calling a model on every event introduces hundreds of
   milliseconds of round-trip per observation. Over a 12-hour session with
   thousands of events, this adds up to a system that feels sluggish and
   that misses fast-evolving problems.
3. **Reliability.** An LLM-as-supervisor inherits the LLM's failure modes:
   hallucinated state, inconsistency across calls, occasional refusals.
   For a process whose job is to detect and recover from failures elsewhere
   in the system, this is the wrong dependency to take on.

Most observations during an orchestration run are trivial: an event arrived,
the event is well-formed, the worker is still within its budget, no
intervention required. A negligible fraction of observations require
human-style judgment: this worker has been quiet for 20 minutes — is it
stuck or doing a long computation? The cost-benefit of an LLM here is
backwards: 99% of the work is mechanical, but 100% of the work is paying
LLM-call rates.

## Decision

**The supervisor (`brehon-supervisor`) is a deterministic Rust process that
consumes an append-only event store. It only invokes an LLM when a judgment
call requires it — typically supervisor-level escalation decisions.**

Concretely:

- The supervisor is one `tokio::spawn` task in the same process as the
  orchestrator. It does not run as a separate agent.
- It reads events from `EventStore` (implemented by `brehon-store-fjall`)
  via a streaming subscription. New events are pushed; recovery on startup
  replays the log to rebuild in-memory state.
- It composes five deterministic components:
  - `EventMonitor` — dispatches events to detectors.
  - `StuckDetector` — flags workers idle past a configured timeout.
  - `BudgetTracker` — accounts token + wall-clock budget per worker and run.
  - `EscalationManager` — decides when a situation warrants escalation.
  - `NudgeGenerator` — composes targeted prompts to send through
    `AgentGateway`.
- LLM calls happen through `DecisionEngine` (a port trait). The supervisor
  only calls it when the escalation manager flags a situation that needs
  human-style judgment.
- Most of the supervisor's work is zero-token: event reads, threshold
  comparisons, lease tracking, budget arithmetic.

The supervisor is therefore the cheap, always-on component. It can poll
events at arbitrarily high frequency without affecting cost, and its
responsiveness is bounded by the event-store read latency (microseconds)
rather than model latency (hundreds of milliseconds to seconds).

## Consequences

**Accepted:**

- The supervisor is constrained to the kinds of decisions you can encode
  in Rust. Sophisticated reasoning ("is this worker actually making
  progress, given the diff so far?") is delegated to the agents themselves
  (via reviewer panels) and to the rare `DecisionEngine` call.
- New supervisor capabilities require new Rust code. We cannot simply
  prompt the supervisor into new behavior.
- The event-store schema is load-bearing. Adding a new dimension of
  supervision requires extending event shapes in `brehon-types` and
  updating the relevant detectors.

**In exchange:**

- The supervisor's marginal cost per observation is approximately zero.
  A user can run Brehon for hours and only pay for what the workers and
  reviewers actually generate, not for the watchdog watching them.
- The supervisor's behavior is testable. Stuck detection, budget
  tracking, and escalation are all covered by deterministic unit tests
  in `brehon-supervisor` (122 tests at the time of this ADR). An
  LLM-based supervisor would require either model mocking (hides bugs)
  or live model calls in CI (expensive and flaky).
- Crash recovery is straightforward: replay events, rebuild state. No
  conversation history to reconstruct.
- The supervisor's responsiveness is bounded by event-store latency
  (microseconds), which is necessary for stuck-detection to fire on
  short windows.

## Alternatives considered

**LLM-as-supervisor (the common pattern).** Rejected for cost, latency,
and reliability reasons enumerated in the context. The break-even
analysis: at ~3 events per second across a fleet of 5 workers during
active development, an LLM-call-per-event supervisor would cost more
than the workers themselves.

**Hybrid: Rust monitor, LLM for *every* nudge decision.** Considered.
Rejected because most nudges are trivially decidable (a worker has been
silent past its timeout — nudge with "are you still working?"). Calling
an LLM for every nudge wastes tokens on cases the rule could decide.
The current design only escalates to the `DecisionEngine` for cases
the rule layer cannot decide.

**External supervisor process.** Considered for crash isolation. Rejected
because the supervisor needs low-latency access to the event store and
to the in-process `AgentGateway` to issue nudges. The crash-isolation
benefit was small (the supervisor reads, it does not mutate worker
state); the latency cost was large.

**No supervisor at all; let agents self-supervise.** Considered briefly.
Rejected because the kinds of failures that matter most (stuck workers,
budget exhaustion, lease leakage) are exactly the failures where the
worker is least able to notice on its own.

## See also

- `crates/brehon-supervisor/src/lib.rs` — supervisor entry point.
- `crates/brehon-supervisor/src/event_monitor.rs` — event-stream consumer.
- `crates/brehon-ports/src/event_store.rs` — the event-store port.
- `crates/brehon-ports/src/decision.rs` — the `DecisionEngine` port.
- [ADR-0006](0006-fjall-tantivy.md) for the event-store backend.

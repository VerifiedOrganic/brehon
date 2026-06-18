# ADR-0009: panic=unwind is load-bearing; panic="abort" is forbidden

**Status**: Accepted
**Date**: 2026-06-18
**Deciders**: project founders

---

## Context

Brehon runs **unattended for hours-to-days**, spending real money driving
AI coding agents. The overriding reliability goal is "never crash, never
wedge": a single bad iteration must not end a multi-day session.

To get that property, Brehon deliberately installs *panic firewalls* at the
seams where untrusted or fallible work happens. Each firewall catches a
panic, logs it at `error!`, and lets the surrounding always-on loop (or the
next request) continue instead of unwinding out and killing the session.

The firewalls in the shipped binary today:

- **MCP tool dispatch** — `crates/brehon-mcp/src/server.rs` wraps each tool
  invocation in `AssertUnwindSafe(tool.execute(args)).catch_unwind().await`.
  A panicking tool returns an error to the caller; the server stays up.
- **TUI per-tick guards** — `crates/brehon-tui/src/run/event_loop.rs` wraps
  the periodic stall-detection and budget ticks, plus the untrusted-event
  supervisor-reset seam, in `std::panic::catch_unwind`. A panic in one tick
  logs and the run loop keeps going. The budget kill-switch is latched
  idempotent, so re-running it next tick after a caught panic is safe.
- **Mux ACP event bridge** — `crates/brehon-mux/src/mux/events.rs` wraps the
  pure per-event formatting in `catch_unwind` so one malformed agent event
  cannot kill a pane's event forwarding.
- **Daemon background tasks** — JoinError / `catch_unwind` monitoring on the
  spawned heartbeat, command-inbox, and detection loops so a panicked task
  is surfaced (and restarted where safe) rather than silently dead.

Every one of these firewalls relies on the panic **unwinding** so the catch
point can observe it. Cargo's default `panic` strategy for the release
profile is `unwind`, but the `[profile.release]` block had no explicit
`panic` key documenting that the project *depends* on it. A future
build-size or "make release smaller/faster" change could set
`panic = "abort"` with no test failure and no obvious symptom — and silently
convert every caught panic into a whole-process abort, ending the unattended
run on the first panic that the firewalls were built to absorb.

## Decision

**`panic = "unwind"` is pinned in `[profile.release]` and `panic = "abort"`
is forbidden for any Brehon profile.**

Concretely:

- The root `Cargo.toml` `[profile.release]` block sets `panic = "unwind"`
  explicitly, with a comment listing the firewalls that depend on it.
- `[profile.release-fast]` inherits `release`, so it gets the same setting;
  no separate key is needed there.
- A reviewer who wants the binary-size / speed win of abort-on-panic must
  first delete the firewall sites above (and accept that any panic ends the
  session). That trade is explicitly rejected for an unattended,
  money-spending orchestrator.

## Consequences

**Accepted:**

- Slightly larger binary and no abort-on-panic codegen win. Unwinding tables
  stay in the binary.
- A caught panic leaves partially-mutated in-memory state behind. This is
  sound only because the wrapped scopes mutate plain scalars/maps with no
  cross-field invariant a half-completed mutation would violate, hold no
  `std::sync::Mutex` across the boundary (so no poison-on-panic), and the
  supervisor uses `parking_lot` locks (no poisoning). New firewalls must
  preserve that property or set a "poisoned" flag and stop instead.

**In exchange:**

- Bounded blast radius: one panicking tool call, tick, agent event, or
  background iteration logs and the session survives.

## Alternatives considered

**`panic = "abort"` for a smaller/faster binary.** Rejected: it would defeat
every `catch_unwind` firewall and turn the first absorbed panic into a
session-ending crash, directly contradicting the never-crash/never-wedge
goal.

**Leave the default (no explicit key).** Rejected: the dependency was
invisible. An optimizer could flip it to `abort` with no failing test. The
explicit key plus this ADR make the dependency reviewable.

## See also

- `crates/brehon-mcp/src/server.rs` — MCP tool-dispatch firewall.
- `crates/brehon-tui/src/run/event_loop.rs` — TUI per-tick / run-loop firewalls.
- `crates/brehon-mux/src/mux/events.rs` — mux ACP event-bridge firewall.
- `crates/brehon-daemon/src/lib.rs` — daemon background-task panic monitoring.
- `Cargo.toml` `[profile.release]` — the pinned `panic = "unwind"`.

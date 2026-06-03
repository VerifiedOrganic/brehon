# ADR-0001: Rust workspace with hexagonal (ports-and-adapters) layout

**Status**: Accepted
**Date**: 2026-04-01
**Deciders**: project founders

---

## Context

Brehon orchestrates long-running multi-agent coding sessions. It has to do
several things at once and do them reliably:

- Spawn and supervise child PTY processes that may live for hours.
- Maintain persistent state across crashes.
- Serve an MCP API to agents in real time.
- Render a TUI without blocking the orchestration loop.
- Coordinate multi-agent review with strict scoring policies.
- Integrate work into a git repository safely, with crash recovery.

The system has to run on a developer laptop, start fast, idle cheaply, and
remain stable for 12+ hour sessions. It must also be testable without spinning
up real LLMs, since the cost and nondeterminism of agent calls would make CI
impractical.

Two questions drove this ADR:

1. **What implementation language?**
2. **What internal architecture style?**

The candidate languages were Rust, Go, TypeScript (Node or Bun), and Python.
The candidate architectures were a flat module layout, a layered (n-tier)
layout, hexagonal (ports-and-adapters), and an actor-model approach.

## Decision

**The codebase is a Rust workspace (currently 36 `brehon-*` crates) organized
as a hexagonal (ports-and-adapters) architecture.**

The split:

- **Core crates** depend only on `brehon-types` (vocabulary), `brehon-ports`
  (trait definitions), and other core crates. They contain pure domain logic.
  Examples: `brehon-orchestrator`, `brehon-supervisor`, `brehon-review`,
  `brehon-config`, `brehon-detect`, `brehon-policy`.
- **Adapter crates** implement port traits against concrete technologies.
  Examples: `brehon-store-fjall` implements `EventStore`/`RunStore`/`ProofStore`
  against fjall; `brehon-git` implements `GitOperations` against libgit2;
  `brehon-mcp` implements MCP serving against `rmcp`.
- **`brehon-cli`** is the only binary. It wires core to adapters at process
  startup and owns the runtime.

The hexagonal seam is enforced by Cargo dependencies: a core crate that
imports an adapter crate fails review.

## Consequences

**Accepted:**

- Long compile times. A clean build is in the tens of seconds on a modern
  laptop; clean releases longer. Incremental builds are fast.
- A steeper learning curve for contributors who do not know Rust.
- More boilerplate around async traits, lifetimes, and error types than a
  garbage-collected language would require.
- A larger LOC count than equivalent Python or TypeScript would have produced.

**In exchange we get:**

- Memory safety and no GC pauses for a long-running supervisor that has to
  remain responsive over many hours.
- A single deployable binary with no runtime dependencies (no Node, no Python
  virtualenv).
- Strong type-driven refactoring. The MCP tool surface, the orchestrator,
  the review state machine, and the storage layer all enforce their invariants
  through the type system; changes ripple cleanly.
- A predictable performance profile for the supervisor's event-loop, which
  consumes a high-throughput event store and must not stall.
- The hexagonal layout means the storage backend, search backend, git layer,
  and agent transport can each be replaced by writing a new adapter crate.
  No core logic needs to change.
- Tests run without real LLMs. Adapter ports are mocked in core crate tests;
  integration tests use real fjall/git on temp dirs.

**Cost of the hexagonal layout specifically:**

- Indirection: a code path that ultimately writes to fjall passes through
  the `EventStore` trait. This is a small cognitive overhead and a small
  compile-time cost (mostly elided by inlining).
- More crates than a flat layout. We accept this for compile parallelism
  and explicit dependency boundaries; Cargo's incremental compilation makes
  the per-crate cost low.

## Alternatives considered

**Go** was a strong contender. Excellent concurrency primitives, fast build,
solid stdlib, easy single-binary distribution. Rejected primarily because:

- The review and supervisor state machines have intricate invariants that
  benefit from sum types and exhaustive matching. Go's lack of sum types
  forces interface-and-type-switch patterns that are easy to get wrong.
- Generic-trait-style polymorphism is awkward in Go; the hexagonal seam
  would have been expressed less crisply.
- The terminal emulation work uses unsafe C bindings (ghostty) for which
  Rust's tooling is more direct.

**TypeScript (Bun or Node)** would have given the fastest start but was
rejected because the supervisor has to remain responsive across a 12-hour
session and we did not want to bet long-session stability on a garbage
collector under unpredictable load. The MCP server's serialization budget
would also have been tighter.

**Python** was rejected on the same long-session and GC grounds, plus the
complexity of producing a single-binary distribution for end users.

**Flat / layered architecture** in Rust was considered for the smaller
crate count. Rejected because the layered approach makes it easier to
let an upper layer pull in lower-layer concrete types — exactly the
coupling the hexagonal seam prevents. We wanted the seam enforced by the
type system and the dependency graph, not by convention.

**Actor model** (e.g. `actix`) was considered for the supervisor's
event-driven nature. Rejected because we already use Tokio for the rest
of the system and did not want two concurrency models in one process.
The supervisor's event loop is implemented as a single `tokio::spawn`
that reads from the event store; this is close enough to an actor in
behavior without the framework overhead.

## See also

- The port traits: `crates/brehon-ports/src/lib.rs`.
- The dependency graph: `cargo tree -p brehon-orchestrator --depth 1`.
- [ADR-0006](0006-fjall-tantivy.md) for the storage adapter choice.

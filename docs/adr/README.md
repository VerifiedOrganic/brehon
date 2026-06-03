# Architecture Decision Records

This directory contains the durable record of significant architectural
decisions in Brehon. Each ADR describes a single decision: the context that
forced it, what was chosen, what was rejected, and the consequences that
followed.

ADRs are immutable once accepted. If a decision is later reversed, the
original ADR stays in place with status `Superseded by ADR-NNNN`, and a new
ADR is written explaining the change.

## Format

Each ADR uses this structure:

- **Status** — Proposed / Accepted / Superseded by ADR-NNNN / Deprecated.
- **Context** — The forces at play. What made this decision necessary?
- **Decision** — What was chosen, stated as a single declarative sentence
  followed by the details.
- **Consequences** — What changes because of this decision, including the
  costs you accept.
- **Alternatives considered** — Other options weighed and why they were
  rejected.

## Index

| ADR | Title | Status |
| --- | ----- | ------ |
| [0001](0001-rust-and-hexagonal.md) | Rust workspace with hexagonal (ports-and-adapters) layout | Accepted |
| [0002](0002-acp-and-mcp.md) | ACP for agent lifecycle, MCP for shared context tools | Accepted |
| [0003](0003-rust-native-supervisor.md) | A deterministic Rust supervisor reading an event store, not an LLM | Accepted |
| [0004](0004-reviewer-panel.md) | Multi-reviewer panel with explicit scoring policy | Accepted |
| [0005](0005-git-worktrees.md) | Git worktree per worker for isolation | Accepted |
| [0006](0006-fjall-tantivy.md) | Fjall as the only event store, Tantivy as the derived index | Accepted |
| [0007](0007-in-process-multiplexer.md) | In-process PTY multiplexer with ghostty_vt, not tmux/Zellij | Accepted |
| [0008](0008-local-first.md) | Local-first operation; no cloud dependencies, no telemetry | Accepted |

# Brehon

**A local-first orchestrator for panels of AI coding agents — judged by panels of other AI agents.**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

---

In early Irish law, a **brehon** (*breithem*) was a professional judge who decided
disputes by panel — weighing arguments, applying precedent, and issuing binding
verdicts whose weight depended on the standing of each judge on the bench.
This project does the same for your codebase.

Workers (AI coding agents — Claude Code, Codex, Gemini CLI, OpenCode, and others)
implement tasks in isolated git worktrees. When work is ready, it is sent to a
**panel of reviewer agents** who independently score it against a policy. If the
panel's collective verdict clears the threshold, the work merges. If not, it goes
back for another round, with the same panel — bound to the task — preserving
context across revisions.

A lightweight Rust supervisor watches the whole process by reading an event store.
Tokens are only spent when human-style judgment is needed; everything else is
free deterministic logic.

## What's in the box

This is a 36-crate Rust workspace (~230k LOC, 1,100+ tests) implementing:

- A **task orchestrator** with a kanban-style board, dependency DAG, and worker pool
  reconciliation (`brehon-orchestrator`).
- A **Rust-native supervisor** that consumes an append-only event store, detects
  stuck workers, tracks token budgets, and escalates when necessary
  (`brehon-supervisor`).
- A **reviewer-panel coordinator** with score collection, threshold evaluation,
  feedback consolidation, panel affinity, and stale-detection (`brehon-review`).
- An **MCP server** exposing 50+ tools for agent coordination, memory, rules, skills,
  task management, verification, and factory control (`brehon-mcp`).
- A **PTY-based terminal multiplexer** with embedded terminal emulation (via the
  vendored ghostty VT) and a full ratatui dashboard (`brehon-mux`, `brehon-tui`).
- A **persistent event store** on fjall (LSM-tree) with full-text search over
  memories/rules/skills via tantivy (`brehon-store-fjall`, `brehon-search-tantivy`).
- A **worktree-per-worker** git layer with recovery for stale lockfiles and
  mid-operation states (`brehon-git`).
- Nine **agent adapters** covering Claude Code, Codex, Gemini, GitHub Copilot,
  Kimi, JetBrains Junie, OpenCode, OpenAI HTTP, and a native ACP client.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for a full walkthrough and the
[Architecture Decision Records](docs/adr/) for the reasoning behind each major
choice.

## How a session runs

```
                          ┌───────────────────────────────┐
                          │      brehon-cli  (TUI)        │
                          └──────────────┬────────────────┘
                                         │
        ┌────────────────────────────────┼────────────────────────────────┐
        │                                │                                │
┌───────▼────────┐              ┌────────▼────────┐              ┌────────▼────────┐
│  Orchestrator  │              │   Supervisor    │              │   MCP Server    │
│  task board +  │              │   event-store   │              │  50+ tools for  │
│  DAG + pool    │              │   reader; AI    │              │  agents to call │
│  reconciler    │              │   only on call  │              │                 │
└───────┬────────┘              └────────┬────────┘              └────────┬────────┘
        │                                │                                │
        │      ┌─────────────────────────┴────────────────────────────────┘
        │      │                                  ↑
┌───────▼──────▼─────┐                  ┌─────────┴──────────┐
│   Mux (panes)      │  PTY ◀──────────▶│  Reviewer Panel    │
│   ghostty_vt +     │  worktree        │  brehon-review     │
│   ratatui          │  per worker      │  scoring + gates   │
└───────┬────────────┘                  └────────────────────┘
        │
   ┌────┴────┬────────┬─────────┬────────┐
   ▼         ▼        ▼         ▼        ▼
 Claude   Codex   Gemini    Kimi    OpenCode  (and others)
```

1. `brehon run` loads config, opens the fjall event store, and reconciles any
   in-flight state from the previous session.
2. The orchestrator computes the task DAG and assigns ready tasks to workers
   based on lane configuration.
3. Each worker gets its own git worktree under `.brehon/worktrees/<worker-id>/`.
   No two workers can step on each other.
4. Workers are spawned as PTY processes inside the mux. The TUI shows their
   live terminals; agents talk to the MCP server for shared context.
5. When a worker reports a task ready, the verification tool spawns a
   **reviewer panel** from configured reviewer lanes.
6. Each reviewer scores the work 1–10, attaches structured findings
   (blocking / suggestion / nitpick), and submits independently.
7. The score collector applies the policy: minimum average, minimum individual
   score, no blocking findings, minimum approval count. If met, the supervisor
   integrates the work via git cherry-pick to the epic branch.
8. If not met, the same panel is bound to the task for the next round.
   The policy caps rounds before escalation to a human supervisor.

## Prerequisites

- **Rust** 1.75 or later. Install via [rustup](https://rustup.rs/).
- **Zig** 0.13 (only required if you are building the vendored ghostty
  terminal-emulation bindings from source; pre-built artifacts cover most users).
- **Git** 2.x with worktree support (any modern version).
- At least one supported agent CLI installed:
  Claude Code, Codex, Gemini CLI, OpenCode, Kimi, JetBrains Junie, or GitHub Copilot CLI.
  You can also point Brehon at any OpenAI-compatible HTTP endpoint via the
  `brehon-adapter-openai` adapter.

## Build

```bash
git clone https://github.com/VerifiedOrganic/brehon.git
cd brehon
git submodule update --init --recursive   # vendored ghostty
cargo build --release
```

The binary is built at `target/release/brehon`. Copy it onto your `PATH`:

```bash
install -m 0755 target/release/brehon ~/.local/bin/brehon
```

## Quickstart

```bash
# Initialize Brehon state inside your project repo
cd /path/to/your/project
brehon init

# Run the orchestrator with the default configuration
brehon run

# Or invoke the MCP server only (for use from another agent)
brehon serve

# Inspect runtime state / dashboard
brehon runtime dashboard

# Diagnose missing agents, malformed config, broken worktrees, etc.
brehon doctor
```

## Configuration

Configuration lives at `.brehon/config.yaml`. The default schema (see
`crates/brehon-config/src/defaults.yaml` for the full version) is built
around two concepts:

- **Launchers** — how to spawn a particular agent CLI. Each launcher specifies
  the adapter (`Acp` or `NativeHooks`), the command, and arguments.
- **Lanes** — named bundles of launcher + model + system prompt. Workers,
  supervisors, and reviewers are assigned to lanes, not directly to launchers.

Example excerpt:

```yaml
version: 1

launchers:
  claude:
    adapter: NativeHooks
    command: claude
  codex:
    adapter: Acp
    command: codex
    args: ["app-server"]
  gemini:
    adapter: Acp
    command: gemini
    args: ["--acp"]

lanes:
  claude-supervisor:
    launcher: claude
    model:
      provider: anthropic
      name: claude-sonnet-4-6
  codex-worker:
    launcher: codex
    model:
      provider: openai
      name: gpt-5.3-codex
  claude-reviewer:
    launcher: claude
    model:
      provider: anthropic
      name: claude-opus-4-6
    system_prompt: |
      You are a reviewer. Your job is to evaluate submitted work,
      not to implement it.
```

The supervisor, worker pool sizing, reviewer panel composition, and review
scoring policy are all configured under their respective sections; see the
defaults file and the schema validator (`brehon-config/src/validate/`) for the
authoritative form.

## CLI Reference

| Command                       | Purpose                                                              |
| ----------------------------- | -------------------------------------------------------------------- |
| `brehon run [--workers SPEC]` | Start a full orchestration session (default subcommand).             |
| `brehon serve`                | Run the MCP server in stdio mode for use from an external agent.     |
| `brehon init [--path P]`      | Create `.brehon/` in a project directory with starter config.        |
| `brehon doctor`               | Check that required CLIs, git, and config are healthy.               |
| `brehon config <subcmd>`      | Inspect, validate, or merge config files.                            |
| `brehon test [--live]`        | Run scenario tests; `--live` exercises real agents.                  |
| `brehon runtime <subcmd>`     | Inspect runtime state (dashboard, events, panes).                    |
| `brehon ps` / `brehon kill`   | Process inspection for in-flight runs.                               |
| `brehon task <subcmd>`        | Direct task-board operations (list, get, transition).                |
| `brehon factory <subcmd>`     | Factory-mode worker lifecycle.                                       |
| `brehon import-plan FILE`     | Import an external task plan (JSON) into the board.                  |
| `brehon process <subcmd>`     | Low-level process control.                                           |
| `brehon reset`                | Reset runtime state. Guarded against destroying `main`/`master`.     |
| `brehon clean`                | Clean up stale worktrees and runtime directories.                    |
| `brehon epic-truth`           | Report the current epic-branch ground truth.                         |

Use `brehon <cmd> --help` for the full flag set of each subcommand.

## MCP integration

Brehon ships as an MCP server. Once installed, point any MCP-capable agent at it
using a config like:

```json
{
  "mcpServers": {
    "brehon": {
      "command": "brehon",
      "args": ["serve"]
    }
  }
}
```

See `.mcp.json.example` in this repo for a copy-pasteable starting point.
The tools currently exposed include `agent`, `advisor`, `health`, `research`,
the `*_memory` family, `*_rule` family, `search_skills`, the `*_task` family,
`verification`, plus factory, git-cherry-pick, context-efficiency,
proof-summary, stability, and routing tools.

## Repository layout

```
crates/
  brehon-types         core domain types and event shapes
  brehon-ports         port traits (hexagonal seam)
  brehon-config        YAML schema, loading, validation
  brehon-orchestrator  task board, DAG, worker pool, reconciler
  brehon-supervisor    event-store monitor, stuck detection, budget tracking
  brehon-review        panel, scoring, thresholds, consolidation
  brehon-runtime       in-process event bus
  brehon-workflow      audited dry-run workflow primitives
  brehon-policy        runtime policy gates
  brehon-detect        pattern-based output anomaly detection
  brehon-protocol      factory client/server wire format
  brehon-daemon        sidecar daemon process
  brehon-gatekeeper    epic-level preflight gating
  brehon-host          experimental terminal-host abstraction
  brehon-acp           Agent Communication Protocol (stdio)
  brehon-mcp           Model Context Protocol server (50+ tools)
  brehon-store-fjall   persistent event store (LSM)
  brehon-search-tantivy full-text index for memories/rules/skills
  brehon-git           worktree + branch + cherry-pick operations
  brehon-mux           in-process PTY multiplexer
  brehon-pty           PTY process spawning
  brehon-tui           ratatui dashboard and panes
  brehon-recording     terminal session recording
  brehon-doctor        diagnostics
  brehon-adapter-sdk   shared adapter trait + helpers
  brehon-adapter-claude    Claude Code adapter (PTY-native hooks)
  brehon-adapter-codex     Codex adapter (websocket app-server)
  brehon-adapter-copilot   GitHub Copilot CLI adapter (ACP)
  brehon-adapter-gemini    Gemini CLI adapter (ACP)
  brehon-adapter-junie     JetBrains Junie adapter (ACP)
  brehon-adapter-kimi      Kimi Code adapter
  brehon-adapter-openai    OpenAI-compatible HTTP adapter
  brehon-adapter-opencode  OpenCode adapter
  brehon-native-agent      Brehon-native ACP runtime
  brehon-cli           command-line entry point (binary: brehon)
  brehon-test-harness  shared test fixtures
  ghostty_vt           vendored terminal-emulation bindings
docs/
  ARCHITECTURE.md      detailed system walkthrough
  adr/                 architecture decision records
```

## Development

```bash
cargo test --workspace                          # full test suite (~1,100 tests)
cargo test -p brehon-orchestrator               # one crate
cargo clippy --workspace --all-targets          # lints (warning-free required)
cargo fmt --all -- --check                      # formatting check
cargo doc --workspace --no-deps --open          # rustdoc
```

The test suite includes unit tests in each crate plus integration tests under
`crates/brehon-cli/tests/` (`scenarios_tests`, `chaos_tests`, `crash_tests`,
`soak_tests`, `stress_tests`, `epic_integration_tests`, `git_tests`,
`supervised_sidecar`, `review_flow`, and `doctor_integration`).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

This is pre-1.0 software. Crate boundaries, configuration shapes, and on-disk
formats may still change. Pin your version.

## License

[MIT](LICENSE)

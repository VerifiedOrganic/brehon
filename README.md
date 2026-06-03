# Brehon

**A local-first orchestrator for panels of AI coding agents — judged by panels of other AI agents.**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

---

In early Irish law, a **brehon** (*breithem*) was a professional judge who decided
disputes by panel — weighing arguments, applying precedent, and issuing binding
verdicts whose weight depended on the standing of each judge on the bench.
This project does the same for your codebase. We just gave the idea a token
budget and a terminal UI.

Here's the loop. **Workers** (AI coding agents — Claude Code, Codex, Gemini CLI,
OpenCode, Kimi, and others) implement tasks in isolated git worktrees. When work
is ready, it goes to a **panel of reviewer agents** who independently score it
against a policy. Clear the threshold and it merges onto an epic branch; miss it
and the task goes back for another round. Presiding over all of it is the
**supervisor** — which is really two things sharing a name: an AI lead that plans
and assigns work, and a deterministic Rust supervision loop that watches an
event store, tracks budgets, and nudges workers that wander off, calling on an
AI itself only for the genuinely ambiguous judgment calls. The principle: spend
tokens on judgment, run everything else as free deterministic logic.

If you're going to use this, **read the [User Guide](docs/USER_GUIDE.md) first.**
It's honest about what this costs and how to make it cost less, and it'll save
you from learning those lessons on a real invoice.

## Heads up before you dive in

- **This works for me. It will, with near-total certainty, have bugs and rough
  edges for you.** Both of those things are true at once, and I'd rather say so
  plainly than oversell it. It's the fifth internal rewrite of the idea, and I
  run it hard: long unattended sessions, *days* on end, against genuinely large
  software. It plans its own follow-up work, picks the next task, sends it to the
  panel, and keeps going — including self-referential tasks where Brehon is the
  thing being worked on, improving itself while I'm asleep. So I'm confident the
  long-horizon loop holds up; that part is well-tested by simply living in it.
  What I *can't* promise you is polish. Something built around one person's
  workflow, run daily by that one person, accumulates sharp corners the author
  has long since stopped noticing — and the *shape* of it (the lane model, the
  panel-judges-panel structure, the rhythm of a session) is tuned to how I work,
  not how you do. It might fit you perfectly, it might be overkill, it might be
  subtly wrong for your style. Treat this repo as a working data point, not a
  finished product.
- **It is built to spend tokens, and it can spend a lot of them.** A panel of N
  reviewers means every "ready for review" event triggers N independent agent
  calls; multiply by revision rounds, multiply again if your worker lane is also
  a paid model, and add an always-on AI supervisor and optional research briefs
  on top. This is not a bug — buying several independent expert opinions on every
  change is the entire point, and it's the thing that makes it expensive. **But
  every part of that is a dial you set:** panel size, review rounds, which models
  on which lanes, budget caps. The defaults shown in the docs are *one person's*
  settings, tuned for verdict quality, not thrift. The [User Guide](docs/USER_GUIDE.md)
  does the real cost arithmetic and lists every knob — turn them down before you
  point this at a real epic.
- **It's CLI all the way down.** There's no web app and no hosted dashboard.
  Brehon drives real agent CLIs as subprocesses and the whole thing runs in your
  terminal. That's deliberate — it's built for long-horizon work driven from the
  command line. If that's your habitat, welcome.

## What's in the box

This is a 36-crate Rust workspace (~230k LOC, 1,100+ tests) implementing:

- A **task orchestrator** with a kanban-style board, dependency DAG, and worker pool
  reconciliation (`brehon-orchestrator`).
- A **Rust-native supervision loop** that consumes an append-only event store,
  detects stuck workers, tracks token budgets, and escalates when necessary
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
- Nine **agent adapters** (`brehon-adapter-*`) covering Claude Code, Codex,
  Gemini, GitHub Copilot, Kimi, JetBrains Junie, OpenCode, OpenAI-compatible
  HTTP, and Google Antigravity — plus a Brehon-native ACP runtime
  (`brehon-native-agent`).

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for a full walkthrough and the
[Architecture Decision Records](docs/adr/) for the reasoning behind each major
choice.

## How a session runs

![Brehon Architecture](docs/images/architecture.svg)

1. `brehon run` loads config, opens the fjall event store, and reconciles any
   in-flight state from the previous session.
2. The orchestrator computes the task DAG and assigns ready tasks to workers
   based on lane configuration.
3. Each worker gets its own git worktree, by default outside the repository
   under the platform data directory, scoped by repo name and hash. No two
   workers can clobber each other's working state.
4. Workers are spawned as PTY processes inside the mux. The TUI shows their
   live terminals; agents talk to the MCP server for shared context.
5. When a worker reports a task ready, the verification tool spawns a
   **reviewer panel** from configured reviewer lanes.
6. Each reviewer scores the work 1–10, attaches structured findings
   (blocking / suggestion / nitpick), and submits independently.
7. The score collector applies the policy: minimum average, minimum individual
   score, no blocking findings, minimum approval count. If met, the work is
   integrated via git cherry-pick to the epic branch.
8. If not met, the same panel roster is bound to the task for the next round,
   up to a configured cap before escalation to a human.

Note that overlapping changes can still conflict when they're integrated onto the
epic branch — isolation prevents live collisions, not merge-time ones. Good plan
design (disjoint write scopes) is what keeps that cheap; the
[User Guide](docs/USER_GUIDE.md) covers how.

## Prerequisites

- **Rust** 1.75 or later. Install via [rustup](https://rustup.rs/).
- **Zig** 0.15.2+ (only required if you are building the vendored ghostty
  terminal-emulation bindings from source; pre-built artifacts cover most users).
- **Git** 2.x with worktree support (any modern version).
- At least one supported agent CLI installed:
  Claude Code, Codex, Gemini CLI, OpenCode, Kimi, JetBrains Junie, or GitHub Copilot CLI.
  You can also point Brehon at any OpenAI-compatible HTTP endpoint via the
  `brehon-adapter-openai` adapter. Whichever you choose, make sure the bare CLI
  can reach its model on its own before you ask Brehon to drive it.

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

# Verify your CLIs, git, and config are healthy before spending anything
brehon doctor

# Load a plan document into the task board (see the User Guide for the format
# and why this is two commands)
brehon extract-plan PLAN.md --output .brehon/plan.json
brehon import-plan .brehon/plan.json

# Run the orchestrator
brehon run

# Or invoke the MCP server only (for use from another agent)
brehon serve
```

Don't have a plan document yet? You don't need one to start. `brehon run` will
spin up with an empty board, and you can create a single task with
`brehon task create` or drive Brehon entirely via an external agent over MCP.
The [User Guide](docs/USER_GUIDE.md) walks a single task end to end as a first
run — start there.

## Configuration

Configuration lives at `.brehon/config.yaml`. `brehon init` generates a clean,
budget-safe starter: a single **active** Claude worker, judged by a
single-member Claude review panel, coordinated by a Claude supervisor. Every
other agent CLI it finds on your `PATH` also gets a launcher plus
supervisor/worker/reviewer lanes written into the file — *inactive* — so turning
one on is a one-line edit under `roles`/`review` (see the `TURN ON ANOTHER AGENT`
section the generated file prints). The schema (see
`crates/brehon-config/src/defaults.yaml` for the full version) is built around
two concepts:

- **Launchers** — how to spawn a particular agent CLI. Each launcher specifies
  the adapter kind (`Acp` for ACP-compatible agents, `NativeAgent` for the
  Brehon-native runtime, `PtyHooks` for Claude's PTY-hook integration, plus
  `Codex`, `Kimi`, and others — see the `AdapterKind` enum), the command, and
  arguments.
- **Lanes** — named bundles of launcher + model + system prompt + reasoning
  effort. Workers, supervisors, and reviewers are assigned to lanes, not directly
  to launchers — which is what lets you route cheap work to cheap models and
  reserve the expensive lane for the scary tasks.

```yaml
version: 1

launchers:
  claude:
    adapter: Acp
    command: claude

lanes:
  claude-supervisor:
    launcher: claude
    model:
      provider: anthropic
      name: claude-opus-4-6
  claude-worker:
    launcher: claude
    model:
      provider: anthropic
      name: claude-sonnet-4-6
  claude-reviewer:
    launcher: claude
    model:
      provider: anthropic
      name: claude-opus-4-6
    system_prompt: |
      You are a reviewer. Evaluate submitted work; do not implement it.
```

That's the active roster (shown here Claude-only for brevity). If you also have
Codex, Gemini, etc. on your `PATH`, `brehon init` writes their launchers and
lanes too — defined but unused. Bringing one into a two-vote panel is then a
matter of adding its `*-worker`/`*-reviewer` lane to `roles`/`review` and bumping
`min_approvals`, not authoring config from scratch.

Panel composition, worker pool sizing, review scoring policy, routing, research,
and budget caps all live under their respective sections. **Every one of them is
yours to tune** — the [User Guide](docs/USER_GUIDE.md)'s "Turning the dials"
section is a tour of the cost and quality knobs, and the schema validator
(`brehon-config/src/validate/`) is the authoritative form.

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
| `brehon task <subcmd>`        | Direct task-board operations (create, list, get, transition).        |
| `brehon factory <subcmd>`     | Factory-mode worker lifecycle.                                       |
| `brehon extract-plan FILE`    | Normalize a plan document into JSON (direct parse or LLM-extract).   |
| `brehon import-plan FILE`     | Import a plan (markdown or normalized JSON) into the task board.     |
| `brehon process <subcmd>`     | Low-level process control.                                           |
| `brehon reset`                | Reset runtime state. Guarded against destroying `main`/`master`.     |
| `brehon clean`                | Remove all Brehon artifacts. Guarded against protected branches.     |
| `brehon maintenance`          | Report or prune stale worktrees and branches.                        |
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
The tools exposed include `agent`, `advisor`, `health`, `research`,
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
  brehon-policy        runtime policy gates
  brehon-detect        pattern-based output anomaly detection
  brehon-protocol      factory client/server wire format
  brehon-daemon        in-process runtime coordination plane
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
  brehon-adapter-claude    Claude Code adapter
  brehon-adapter-codex     Codex adapter (websocket app-server)
  brehon-adapter-copilot   GitHub Copilot CLI adapter (ACP)
  brehon-adapter-gemini    Gemini CLI adapter (ACP)
  brehon-adapter-junie     JetBrains Junie adapter (ACP)
  brehon-adapter-kimi      Kimi Code adapter
  brehon-adapter-openai    OpenAI-compatible HTTP adapter
  brehon-adapter-opencode  OpenCode adapter
  brehon-adapter-agy       Google Antigravity (agy) adapter
  brehon-native-agent      Brehon-native ACP runtime
  brehon-cli           command-line entry point (binary: brehon)
  brehon-test-harness  shared test fixtures
  ghostty_vt           vendored terminal-emulation bindings
docs/
  USER_GUIDE.md        the practical guide — start here
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

See [CONTRIBUTING.md](CONTRIBUTING.md) for how contributions are handled, the
hexagonal dependency rules, and what's likely to land versus get a long
discussion first.

## License

[MIT](LICENSE)

This is pre-1.0 software. Crate boundaries, configuration shapes, and on-disk
formats may still change. Pin your version.

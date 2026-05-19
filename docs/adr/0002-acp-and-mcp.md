# ADR-0002: ACP for agent lifecycle, MCP for shared context tools

**Status**: Accepted
**Date**: 2026-04-05
**Deciders**: project founders

---

## Context

Brehon orchestrates a heterogeneous fleet of agent CLIs — Claude Code,
Codex, Gemini, OpenCode, Copilot, Kimi, Junie, plus a brehon-native runtime
for OpenAI-compatible providers. Each agent ships with its own conventions
for how it is invoked, how prompts are delivered, how output is captured,
and how it exposes tools to the model.

Two distinct concerns have to be solved:

1. **Agent lifecycle and bidirectional messaging.** How does the orchestrator
   start an agent, deliver prompts, observe completion, send mid-session
   commands, and detect crashes?
2. **Shared context and structured tools.** How do agents query shared memory,
   look up project rules, transition tasks, and submit reviews — in a way
   that is uniform across every agent type?

Solving (1) and (2) with the same protocol would let any agent participate
without bespoke per-agent integration code, but it would also require every
agent vendor to implement the same protocol. As of 2026, no such universal
protocol exists in practice.

The available protocols at the time of this decision:

- **ACP (Agent Client Protocol)** — JSON-RPC over stdio for agent session
  lifecycle. Implemented natively or via shims by Gemini CLI, OpenCode,
  GitHub Copilot CLI, JetBrains Junie, and several others. Lifecycle
  primitives: session_start, prompt, prompt_response, message, completion.
- **MCP (Model Context Protocol)** — JSON-RPC over stdio (or HTTP) for
  exposing tools and resources to a model. Implemented broadly by client
  agents and increasingly by hosting platforms.
- **A2A (Google Agent-to-Agent)** — Federation protocol for cross-system
  agent calls. Interesting future direction but not the agent-CLI
  control-plane that current coding agents speak.
- **Native CLI conventions** — Each CLI's own way of taking prompts (stdin,
  file arg, websocket app-server, etc.).

## Decision

**Brehon uses ACP as the primary agent-lifecycle protocol and MCP as the
shared-context tool protocol. Both speak stdio JSON-RPC. A small number of
agents that do not speak ACP are wrapped by per-CLI adapters that translate
their native protocol into the same `AgentAdapter` trait.**

Concretely:

- The `brehon-acp` crate implements the ACP wire protocol (session,
  message, prompt, completion).
- The `brehon-adapter-sdk` crate defines the `AgentAdapter` trait
  (`crates/brehon-adapter-sdk/`). Each agent-specific crate
  (`brehon-adapter-claude`, `brehon-adapter-codex`, etc.) implements that
  trait, regardless of the underlying transport.
- ACP-native agents (Gemini, Copilot, Junie, OpenCode, brehon-native-agent)
  use `brehon-acp` directly.
- Non-ACP agents are wrapped:
  - **Claude Code** uses its NativeHooks transport (the CLI's built-in hook
    mechanism). The adapter still implements `AgentAdapter`; the bytes on
    the wire are different.
  - **Codex** uses a websocket connection to `codex app-server`.
  - **OpenAI-compatible HTTP** uses a streaming HTTP client.
- The `brehon-mcp` crate is the MCP server built on `rmcp`. Every agent —
  ACP or otherwise — calls Brehon's MCP server to read shared memory,
  manage tasks, submit reviews, and so on.

The configuration model in `brehon-config` mirrors this split: each lane
declares a `launcher` (which carries the adapter / transport), separate
from the model and prompt configuration. Adding a new agent type is a new
launcher and a new adapter crate; the rest of the system is unchanged.

## Consequences

**Accepted:**

- Two protocols to maintain. We test both extensively in the adapter crates.
- ACP-shaped APIs leak into the abstraction even for non-ACP agents,
  because `AgentAdapter` reflects ACP's lifecycle (session, prompt,
  completion). For Claude (NativeHooks) and Codex (websocket), we translate.
- The MCP server must run inside the same process as the orchestrator
  (`brehon run`) when the verification, task, and factory tools need to
  hit live in-process state. A standalone `brehon serve` covers
  context-only use cases for external agents.

**In exchange:**

- Adding a new agent CLI is bounded work: one adapter crate, one launcher
  entry in config, no changes to the orchestrator, supervisor, review, or
  MCP server.
- Agents see a uniform tool surface regardless of how they were spawned.
  A Claude reviewer and a Codex reviewer call the same
  `verification.submit_review` tool with the same schema.
- The transport split (ACP for lifecycle, MCP for tools) matches industry
  reality. Agents that already implement ACP work with zero code changes
  beyond a launcher entry.
- Crash recovery is uniform. ACP gives us a clear session-end signal;
  websocket adapters expose the same signal through their wrapper; the
  supervisor consumes the same events regardless.

## Alternatives considered

**MCP only, no ACP.** MCP is excellent for tools but does not address
agent lifecycle, prompt delivery to a child process, or session
termination. Trying to make MCP carry agent control would have required
non-standard extensions and broken interop with the agents that already
speak ACP. Rejected.

**ACP only, no MCP.** ACP carries messages and prompts; it is not the right
shape for the catalog of structured tools (memories, rules, skills, tasks,
verification, factory) that need to be discoverable by any model. MCP is
the industry standard for this and is already implemented by every relevant
client. Rejected.

**A2A as a third protocol.** Considered for future-proofing. A2A targets
cross-system agent federation, not the local client-to-CLI control plane.
We deferred. If Brehon ever exposes agents to other systems, A2A is the
likely entry point.

**One bespoke per-agent transport per adapter, no shared protocol.** This
is what most multi-agent systems do today. Rejected because the maintenance
cost grows linearly with each new agent type, the user-facing tool surface
diverges, and review consistency suffers. ACP + MCP forces uniformity.

**Run MCP as a separate process.** Considered, since this is how MCP is
often deployed. Rejected for the `brehon run` path because the
verification/task/factory tools need to hit in-process orchestrator state
in real time, and an IPC hop on every MCP call would add latency and a
crash boundary. The `brehon serve` command supports the separate-process
deployment for use cases that don't need live orchestrator state.

## See also

- `crates/brehon-acp/` — ACP implementation.
- `crates/brehon-mcp/` — MCP server (50+ tools).
- `crates/brehon-adapter-sdk/` — the `AgentAdapter` trait.
- The nine `crates/brehon-adapter-*/` crates.

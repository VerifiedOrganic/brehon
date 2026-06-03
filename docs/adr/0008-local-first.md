# ADR-0008: Local-first operation; no cloud dependencies, no telemetry

**Status**: Accepted
**Date**: 2026-04-18
**Deciders**: project founders

---

## Context

Multi-agent coding orchestrators in 2026 split broadly into two camps:

- **SaaS**, where the orchestrator runs on the vendor's infrastructure
  and the user's repository state is either uploaded or accessed via
  git push/pull. Examples include the hosted background-agent products
  from several major vendors.
- **Self-hosted**, where the orchestrator runs on the user's
  infrastructure but expects supporting services (database, queue,
  search) to be operated separately.

Both models have real benefits — the SaaS path offers managed reliability;
the self-hosted path offers control without local-machine constraints.
Both also conflict with the use case Brehon was built for:

- A developer working on a private repository on their own machine.
- Sometimes touching code under NDAs or with regulatory constraints
  on where the source can travel.
- Sometimes working offline (flights, trains, network failures).
- Sometimes iterating fast and unwilling to wait on round-trips to a
  cloud orchestrator for every event.
- Not wanting to operate a database, a queue, or a search server
  alongside their editor.
- Not wanting telemetry beacons in their development tooling.

The architectural choices for storage, multiplexing, and supervision
(ADRs 0003, 0006, 0007) have already moved the system toward local-only.
This ADR makes that posture explicit and binding.

## Decision

**Brehon runs entirely on the user's machine. It does not depend on any
cloud service, does not call out to any backend operated by the project,
and emits no telemetry.**

Concretely:

- All persistent state lives under `.brehon/` inside the user's project
  directory. The event store (`brehon-store-fjall`) and search index
  (`brehon-search-tantivy`) are embedded libraries with no remote
  backend.
- The MCP server (`brehon-mcp`) runs in-process or over stdio. It does
  not listen on a network port by default.
- The TUI and multiplexer (`brehon-tui`, `brehon-mux`) are in-process
  and require no external binaries (see [ADR-0007](0007-in-process-multiplexer.md)).
- The git layer (`brehon-git`) operates on local repositories. Brehon
  does not push, pull, or fetch on the user's behalf without an
  explicit command.
- **There is no telemetry, crash reporting, or analytics in the
  binary.** No HTTP request to any project-operated endpoint at any
  time, under any configuration.
- The agent backends — Claude, Codex, Gemini, OpenAI-compatible
  endpoints — necessarily call out to their respective providers when
  the user configures them. That traffic is the user's choice and the
  user's relationship with each provider; Brehon does not interpose,
  log centrally, or proxy.
- Distribution is a single `cargo install`-able binary. No installer,
  no daemon, no system service.

## Consequences

**Accepted:**

- No multi-machine orchestration out of the box. A user who wants to
  run workers on different machines would need to operate `brehon`
  on each machine and coordinate manually (or write an adapter).
- No managed reliability. A user's laptop crashes; the run dies. We
  invest in fast crash recovery (see [ADR-0003](0003-rust-native-supervisor.md))
  to make this acceptable.
- Disk usage scales with session length. The event store grows
  monotonically; we provide `brehon clean` to reclaim space.
- No vendor-side observability. If users hit bugs, we cannot see
  telemetry — they must report. This is a deliberate trade against
  the alternative (collecting data on what users are doing) and we
  consider it the right trade for a developer tool.
- Updates are pull-based. Users update by re-running `cargo install`
  or downloading a new release. There is no auto-update path.

**In exchange:**

- Source code never leaves the user's machine. Brehon does not move
  the user's repository state anywhere it isn't already going.
- Brehon works offline. After agent providers are configured, the
  only outbound traffic is the user's chosen LLM calls.
- No service to monitor, no rate limits to hit, no privacy policy to
  read. The trust model is: do you trust the binary?
- No vendor lock-in. The on-disk format is documented; the user's
  data is theirs.
- No "platform" risk. The project's commercial fate (if any) does
  not affect users' existing installations.
- Fast iteration. There is no round-trip to a cloud orchestrator;
  every operation is microseconds away.

## Alternatives considered

**Optional cloud sync.** Considered for state synchronization across
machines. Rejected because "optional" cloud features almost always
become "required" over time as features accrue around them. Keeping
the surface purely local prevents that drift.

**Optional anonymous telemetry.** The pattern of an
opt-in "help us improve" beacon. Considered briefly. Rejected because
(a) even opt-in telemetry erodes user trust, (b) we have no analytics
infrastructure and no plan to build one, and (c) any data we collected
would be code-related and therefore sensitive almost by definition.
If we ever need usage data we will ask explicitly through other
channels.

**Crash-reporting service.** Tempting given the complexity of the
multiplexer and the long-running supervisor. Rejected for the same
trust reasons. We invest instead in fast crash *recovery* and in
making local debug output (`brehon doctor`, the event log under
`.brehon/store/`) sufficient for users to report bugs themselves.

**Cloud-hosted MCP server option.** Considered as a deployment
convenience. Rejected because the verification/task/factory tools
need in-process orchestrator state to work correctly (see
[ADR-0002](0002-acp-and-mcp.md)). The `brehon serve` stdio mode
covers external-agent context-only use cases without needing a
network deployment.

**Multi-machine worker pool.** Considered for users with beefy
remote compute. Deferred. The architecture does not preclude it —
the `AgentGateway` port could be implemented over a network transport,
and adapters could spawn remote agents — but we have not built it
and do not plan to until the local case is fully solid.

## See also

- [ADR-0006](0006-fjall-tantivy.md) — embedded storage choice.
- [ADR-0007](0007-in-process-multiplexer.md) — in-process multiplexer.
- [ADR-0003](0003-rust-native-supervisor.md) — in-process supervisor.
- `crates/brehon-cli/src/commands/clean.rs` — local cleanup tooling.
- `crates/brehon-doctor/` — local diagnostics.

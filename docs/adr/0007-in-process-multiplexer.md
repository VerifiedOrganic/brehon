# ADR-0007: In-process PTY multiplexer with ghostty_vt, not tmux/Zellij

**Status**: Accepted
**Date**: 2026-04-16
**Deciders**: project founders

---

## Context

Brehon spawns multiple long-running agent CLI processes side-by-side and
needs to:

- Capture their terminal output continuously, in real time.
- Render their output in a TUI dashboard alongside task state, review
  state, and event logs.
- Inject prompts directly into each process's stdin (a real keystroke
  stream, not a synthetic message — many CLIs read from the terminal
  in non-line-buffered mode).
- Track each pane's lifecycle (spawning, running, exited, crashed).
- Recover from individual pane crashes without taking the whole UI down.
- Run inside one process so the supervisor, orchestrator, MCP server,
  and TUI all share the same in-memory state.

The well-trodden path for "multiple terminals in one window" is to shell
out to `tmux` or `zellij` and let those handle session management. This is
attractive because they are mature, battle-tested, and users may already
have keybinding muscle memory for them.

But there are real costs to that approach for Brehon's specific use case:

- **IPC complexity.** Coordinating between Brehon (which owns the
  orchestrator state) and tmux (which owns the panes) requires a
  bidirectional control protocol over a control socket. Every pane
  operation becomes a round-trip.
- **State split-brain.** Pane state lives in tmux; orchestrator state
  lives in Brehon. Keeping these consistent during crashes and
  reconnects is non-trivial.
- **Dependency on an external binary** that must be installed,
  available on `PATH`, and at a compatible version. Users without tmux
  cannot run Brehon.
- **No direct access to PTY buffers.** Reading "what is the worker's
  current screen state?" requires asking tmux. Programmatic prompt
  injection is awkward (tmux `send-keys` has its own quoting and
  buffering semantics).
- **UI constraints.** Mixing the TUI's task board, event log, and
  reviewer panels with tmux-owned panes is essentially impossible
  to do cleanly. Brehon would have to run *inside* a tmux pane,
  and tmux's pane layout would constrain Brehon's layout.

## Decision

**Brehon implements its own in-process PTY multiplexer in `brehon-mux`,
using `portable-pty` for PTY spawning, the vendored `ghostty_vt`
bindings for terminal emulation, and `ratatui` for rendering. It does
not depend on tmux, zellij, or any external multiplexer.**

Concretely:

- `crates/brehon-mux/src/lib.rs` is the multiplexer. Each pane is a
  `Pane` with a state machine (spawning / running / exited / crashed).
- Each pane owns:
  - A child process (PTY-attached) via `portable-pty`.
  - A direct write handle to the PTY's master side for prompt
    injection.
  - A terminal-emulator state via `ghostty_vt` (the vendored bindings
    in `crates/ghostty_vt` and `crates/ghostty_vt_sys`), which gives
    us a real VT100/xterm-compatible buffer we can query.
- `crates/brehon-tui/src/` renders panes alongside task and review
  state inside a single ratatui layout. The TUI and the mux share an
  in-process command channel; UI input becomes runtime commands.
- The mux implements `RuntimeCommandPort` (from `brehon-ports`) so
  the rest of the system can request pane-level operations (spawn a
  worker, inject a prompt, capture output) without knowing about PTYs.
- The vendored ghostty submodule lives at `vendor/ghostty`. We pin a
  specific commit; the bindings layer (`ghostty_vt`) presents a stable
  Rust API regardless of ghostty's internal evolution.

`brehon-mux` is the largest single TUI codebase at ~26k LOC with 323
tests covering pane state transitions, prompt-injection edge cases,
and crash recovery.

## Consequences

**Accepted:**

- A significant chunk of code (`brehon-mux` + `brehon-tui` together are
  ~58k LOC) that would not exist if we delegated to tmux. The
  long-term maintenance cost is real.
- We need to keep up with terminal emulation correctness as the
  ecosystem evolves. The vendored ghostty bindings cover this for
  now; we may need to vendor-bump or contribute upstream as new edge
  cases surface.
- Zig is required to build the ghostty bindings from source. We
  document this in the README as a prerequisite for source builds.
  Pre-built artifacts cover the common case.
- Users cannot use their existing tmux keybindings inside Brehon
  panes. The TUI has its own keybinding scheme (with an overlay help
  via `Ctrl-?`).

**In exchange:**

- One process. The supervisor, orchestrator, MCP server, mux, and TUI
  all share in-memory state. No IPC for pane operations.
- Direct programmatic access to PTY buffers. We can query a pane's
  current screen state, scroll back through history, and inject
  prompts without intermediary protocol translation.
- One binary, no external dependencies. `cargo install brehon` and
  go.
- The TUI layout is fully under our control. We can mix panes, task
  board, review queue, event log, and supervisor pane in one
  coherent layout.
- Crash isolation we actually need: a pane crash is reflected
  directly in its `Pane` state and the mux can recover or replace
  it. No control-socket reconnect dance.
- Real input-injection. `Pane::inject_prompt()` writes directly to the
  PTY master. We have caught and tested edge cases (control sequences,
  embedded escapes, partial writes) in our own code rather than
  inheriting tmux's quoting rules.

## Alternatives considered

**Shell out to tmux.** The most obvious path. Rejected for the reasons
in the context: IPC complexity, split-brain state, external dependency,
constrained UI layout. Several earlier prototypes tried this and the
control-protocol complexity dominated the codebase. Replacing it with
the in-process multiplexer simplified things substantially.

**Shell out to zellij.** Same trade-offs as tmux. Zellij has a nicer
plugin model than tmux but it does not help us here — we'd still be
coordinating across a process boundary for every pane operation.

**Embed wezterm's mux library.** Considered. Wezterm has a mature
mux internally but it is tightly coupled to wezterm's GUI rendering;
extracting just the mux without the rest of wezterm is a non-trivial
fork. We could revisit if the maintenance cost of our own mux
becomes burdensome.

**Embed alacritty's vt parser.** Considered. Alacritty's VT parser is
solid but smaller in scope than ghostty's, and alacritty is GPU-focused
in ways that do not benefit a server-side mux. Ghostty's VT was a
closer fit.

**Run Brehon inside tmux.** Considered as a degenerate option. Rejected
for the layout-constraint reason — the TUI needs to render its own
non-pane panels alongside the agent panes, and that's not possible if
tmux owns the outer layout.

**Have an "external multiplexer" mode** as an option. Documented as a
future direction (the `brehon-host` crate is an experiment in this
space — see `HeadlessTerminalHost`), but the default and only fully
supported mode is the in-process multiplexer. We do not want to
maintain both code paths in parallel as first-class options.

## See also

- `crates/brehon-mux/` — the multiplexer.
- `crates/brehon-tui/` — the dashboard and layout.
- `crates/brehon-pty/` — PTY spawning wrapper.
- `crates/ghostty_vt/` and `crates/ghostty_vt_sys/` — vendored
  terminal-emulation bindings.
- `crates/brehon-host/` — experimental headless-host abstraction for
  out-of-process terminal hosts.
- `crates/brehon-ports/src/runtime.rs` — `RuntimeCommandPort`,
  `TerminalHostAdapter`.

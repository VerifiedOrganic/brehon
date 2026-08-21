# ADR-0010: Per-agent `file://` clones and kernel confinement, not worktrees

**Status**: Accepted
**Date**: 2026-08-21
**Deciders**: project founders
**Supersedes**: [ADR-0005](0005-git-worktrees.md)

---

## Context

ADR-0005 chose one git worktree per worker. In extended production use the
isolation it provides has proven to be a *convention*, not a boundary: agents
write outside their worktree, and no guard added since has stopped it.

An investigation in August 2026 (see `docs/research/`) established why, and the
answer is structural rather than a defect in any particular guard.

**A linked worktree is not a self-contained repository.** Its index, refs, logs
and objects live in `<main>/.git`. That forces an unresolvable dilemma for any
confinement layer:

- A write-jail tight enough to protect the project repository **breaks
  `git commit` inside the agent's own worktree** —
  `fatal: Unable to create '<main>/.git/worktrees/<n>/index.lock'`.
- A write-jail loose enough to permit `git commit` **must grant the shared
  object store**, and that grant permits a verified destruction chain:
  `rm -rf <main>/.git/worktrees/<sibling>` → `git branch -D <sibling>` →
  `git reflog expire --all && git gc --prune=now` → the sibling's unmerged,
  panel-approved commits are unrecoverable.

Brehon already makes the second trade in production:
`crates/brehon-adapter-codex/src/codex.rs:1398` pushes both the resolved gitdir
and the resolved commondir into the per-turn Seatbelt policy, so
`<project>/.git/hooks/` is writable by every Codex agent today. The hook-based
RCE into the operator's own checkout is not merely unblocked — it is authorized.

Three further facts constrain any replacement:

1. **`git clone <path>` hardlinks packfiles.** The agent's own
   `.git/objects/pack/*.pack` shares an inode with the source. Packs are mode
   444, so a naive write fails; the documented careless-model response to
   `Permission denied` is `chmod -R u+w .`, after which an in-place write inside
   the agent's *own* directory corrupts the source repository. No confinement
   layer on any platform closes this: the write target is inside the one
   directory that must remain writable, and Landlock cannot restrict `chmod(2)`
   at all. `git clone file://` produces disjoint object stores.
2. **All agents run as one uid with reads deliberately open** (denying reads
   breaks rustc, cargo, node and every vendor CLI). Any credential a transport
   uses — token, key, broker id — is readable by every peer agent. Transport
   cannot authenticate here.
3. **`git fetch` from a hostile repository executes nothing**, verified against
   planted `pre-receive`/`post-receive`/`update`/`post-update` hooks,
   `uploadpack.packObjectsHook`, `core.alternateRefsCommand`, and a partial-clone
   promisor. `git push` *into* one fires six code paths as the pushing user.
   Direction is therefore load-bearing.

## Decision

**Each agent gets its own full git clone seeded over `file://` from a
brehon-owned read-only base mirror; work returns by brehon fetching from the
agent repository into the canonical repository; and every agent process is
confined by the host kernel, fail-closed.**

```
canonical            (brehon-write, agent-deny-write)
  │  git clone --bare file://canonical  base.git
  ▼
base.git             (brehon-write, agent-READ-only, trusted)
  │  git clone file://base.git  <agentdir>
  ▼
agent repo           (agent-write; brehon never cwd's here; agent pushes nowhere)
  │  git -C canonical fetch --no-tags <agentdir> \
  │      'refs/heads/brehon/*:refs/heads/brehon/<agent-id>/*'
  ▼
canonical            cherry-pick -x, reviewer panel, is_safe_brehon_branch unchanged
```

Four rules follow, and each is an invariant the codebase must enforce
mechanically:

- **Seed over `file://`, never a bare path.** Asserted at spawn by walking
  `<agentdir>/.git/objects` for `st_nlink > 1`.
- **Brehon fetches from agent repositories; it never pushes into one, and never
  runs git with an agent repository as cwd or `GIT_DIR`.** Inspection of agent
  repositories goes through libgit2, which runs no hooks, filters, fsmonitor or
  textconv by construction. Mutation happens only in canonical.
- **The agent pushes nowhere.** `base.git` is read-only to the agent; that
  ownership split is what makes brehon's refresh of `base.git` safe.
- **Confinement is required to spawn.** The existing but unused
  `SandboxSpec` type is compiled to a Seatbelt profile on macOS or a Landlock
  ruleset on Linux. Inability to establish a boundary is an error that refuses
  the spawn, unless `unsafe_marker` is explicitly set.

## Consequences

- Isolation becomes a kernel property rather than a convention. An agent that
  attempts to write outside its clone receives `EPERM` regardless of which
  binary, language, or absolute path it used.
- Object stores are disjoint. One agent can no longer destroy a peer's reviewed
  commits, because there is no shared store to garbage-collect.
- Disk cost rises from near-zero to roughly 22 MB per agent on a 700 MB
  repository (~200 MB for eight agents). Seeding costs ~0.5 s per agent.
- `git worktree list` ceases to be the registry of live agent directories.
  Brehon must own that inventory. Given that the current tree has leaked 1572
  worktree directories with 76 of 81 registrations prunable, this makes an
  existing failure visible rather than introducing a new one.
- Two agents can now choose the same branch name without git objecting.
  Destination namespacing on the fetch-back becomes mandatory; without it a
  reviewed commit can be silently lost to a force-update.
- Agents must synchronise with `git merge origin/main`, not `git rebase`.
  Rebase rewrites SHAs, which would let brehon integrate a commit no reviewer
  panel ever saw while `cherry-pick -x` records provenance that looks
  legitimate.
- `crates/brehon-cli/src/commands/run/setup/agent_git_guard.rs` (438 lines)
  becomes dead weight and is deleted — but only after confinement lands, never
  before.
- Windows gains no supported path. `confine_argv` returns an error there and
  names WSL2 and containers in the message.

## Alternatives considered

**Keep worktrees, harden the guard.** Rejected on the dilemma above: no guard
can exist under this layout. The existing 768-line `claude_hook.rs` command
inspector is the experiment already having run — escapes continued.

**Bare canonical repository.** Removes the main checkout as a write target and
nothing else: siblings still enumerate, the common dir is still shared, hooks
are still shared, `gc --prune=now` still destroys peers. Solves the least
valuable third of the problem at the cost of the operator's daily checkout.

**`clone --shared` / `--reference` / alternates.** Inherits the source-gc
corruption hazard, which forces `extensions.preciousObjects`, which makes
`git repack -ad` fatal and `git gc` a silent no-op — all to save 22 MB.

**`ext::` transport to a brehon broker, with `GIT_NAMESPACE`.** The most
elegant option and the clearest rejection. It requires setting
`protocol.ext.allow` away from its deliberate `never` default, re-enabling
`.gitmodules`-driven remote code execution fleet-wide; its per-agent identity is
a string the agent owns, verified by one agent deleting a ref inside another's
namespace; Landlock cannot restrict unix-socket connect until ABI v9
(Linux 7.1), making it the least portable option rather than the most enforced;
unreviewed objects land in canonical before the panel runs; and a brehon restart
breaks every agent's `origin`, teaching the model to repair it by pasting the
canonical path back in.

**Local HTTP with per-agent tokens, or SSH with per-agent keys.** Both place a
credential in a file every peer agent can read. SSH additionally requires
enabling a machine-wide network service to solve a local isolation problem.

**`git daemon`.** Disqualified: the protocol is unauthenticated, and an
anonymous local client was verified deleting a branch in the canonical
repository.

**Bundle exchange.** The only option whose safety does not depend on the
confinement layer, and therefore retained as the recovery and air-gap path — but
its per-agent base-SHA bookkeeping is not worth carrying as the steady state on
macOS or modern Linux.

**Per-agent UNIX uid.** The only complete answer and the only credible Windows
story. Deferred, not rejected: each uid needs its own `$HOME` with working
vendor-CLI authentication, which is a licensing problem as much as a technical
one. Trigger conditions are recorded in `docs/CONTAINMENT_PLAN.md`.

**Containers or VMs per agent.** A complete boundary with the wrong ergonomics:
virtiofs stat-storms on a large Rust workspace, host holes punched back through
for local services, and N duplicated toolchains. The threat model is a careless
model, not a motivated adversary.

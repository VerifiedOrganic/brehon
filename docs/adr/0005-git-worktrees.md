# ADR-0005: Git worktree per worker for isolation

**Status**: Accepted
**Date**: 2026-04-12
**Deciders**: project founders

---

## Context

Brehon runs multiple worker agents concurrently against the same project
repository. Each worker may:

- Run arbitrary shell commands (`cargo build`, `pytest`, `npm install`).
- Create branches.
- Stage and commit changes.
- Rebase, cherry-pick, merge.
- Touch files anywhere in the tree.

If two workers operated on the same working directory, the failure modes
are obvious and catastrophic: one worker's build clobbering another's,
concurrent index updates leaving the repository in an unrecoverable
state, branch checkouts overwriting in-progress work, file watchers
firing on the wrong worker's edits.

The isolation primitive needs to give each worker:

- An independent working directory.
- An independent index and `HEAD`.
- An independent branch.
- Full access to the project's git history (so cloning the entire repo
  per worker is wasteful but possible).
- Cheap creation and cheap teardown.
- A recovery path when the worker crashes mid-operation.

The options:

1. **Full clone per worker** — `git clone` to a fresh directory.
2. **Symlink farms** — symlinks for the source, separate `.git/` per
   worker. Fragile across platforms.
3. **Container-per-worker** — Docker or similar with the repo mounted.
4. **`cp -r` per worker** — copy the working directory.
5. **`git worktree`** — git's native feature for multiple working trees
   sharing one object database.

## Decision

**Each worker runs in its own `git worktree` under Brehon's effective
`orchestration.worktree_root`. The default root lives outside the shared
repo and is scoped by repo name/hash; `.brehon/worktrees/` remains a
legacy location that cleanup and maintenance can read/prune. The
`brehon-git` crate owns creation, cleanup, integration, and crash
recovery.**

Concretely:

- `setup::prepare_scoped_worktrees_with_progress()` (in
  `crates/brehon-cli/src/commands/run/setup.rs`) is invoked at
  `brehon run` startup. It calls `Git2Operations::create_worktree()`
  per worker.
- Each worktree gets a dedicated branch following the convention
  `brehon/<worker-id>/<task-id>`.
- The PTY spawn step sets `cwd` to the worker's worktree, so any
  git command the agent runs operates inside its sandbox.
- Integration uses **cherry-pick**: when a panel approves a worker's
  commits, the supervisor cherry-picks the approved commit range onto
  the epic branch via `brehon-git::MergeOps`/`Git2Operations`.
- `RecoveryOps` handles mid-operation crashes by inspecting
  `CHERRY_PICK_HEAD`, `MERGE_HEAD`, `REBASE_HEAD`, the index, and
  worktree lockfiles. It repairs rather than abort-ing where safe.
- `cleanup_scoped_worktrees()` removes worktrees on normal exit.
- Destructive operations are guarded by `is_safe_brehon_branch`
  (`crates/brehon-cli/src/commands/clean.rs`). The guard refuses to
  touch `main`, `master`, the epic branch, or anything not prefixed
  `brehon/`. It is also resistant to path-traversal exploits and
  unicode confusables; recent test coverage hardened it against
  bypass attempts.

## Consequences

**Accepted:**

- Worktrees share the same object database. A worker that runs
  `git gc --aggressive` could affect other workers' read latency
  (rare, but possible).
- The host filesystem must support multiple working trees of the
  project. For 5 workers on a 1GB repo, this is ~5GB of working-tree
  state under the effective worktree root (objects shared, source
  duplicated).
- Crash recovery code is non-trivial. The `brehon-git` crate has
  62 tests covering stale lockfiles, mid-rebase states, partial
  cherry-picks. This is real complexity.
- Integration is via cherry-pick, not merge commit. The epic branch
  history is linear (one commit per approved task), which we prefer
  for readability and bisect-ability but loses the branchy structure
  of who-did-what-in-parallel.

**In exchange:**

- Cheap creation and teardown. `git worktree add` is fast (no object
  copy) and `git worktree remove` is fast.
- Native primitive. We use git as designed. No emulation layer, no
  symlink fragility, no container runtime dependency.
- Each worker can branch, commit, rebase, push freely without
  affecting any other worker.
- The shared object database means integrating cherry-picks does not
  re-fetch or re-pack objects.
- Crash isolation. A worker that crashes leaves its worktree behind
  in a consistent-enough state that `RecoveryOps` can repair it
  without touching other workers.
- No virtualization or container runtime. Brehon runs on a developer
  laptop without Docker installed.

## Alternatives considered

**Full clone per worker.** Wastes disk for the object store (a 1GB
repo with 5 workers becomes 5GB of objects). `clone` is slow on
large repos. Integration requires `git fetch` between worktrees,
which is much slower than cherry-pick from a shared object store.
Rejected.

**Symlink farms.** Considered for legacy systems without modern
git. Fragile across platforms (Windows symlink permissions, macOS
case-insensitive filesystems, weird `.gitignore` interactions).
Rejected.

**Container per worker.** Considered. Containers give stronger
isolation than worktrees (filesystem, network, process namespace)
but they require a container runtime on the user's machine, add
significant startup latency per worker, and complicate the agent's
access to the project's tooling (compilers, language servers,
package managers). For a local-first developer tool, the runtime
dependency was a non-starter. Worktrees give us the isolation we
actually need without the operational tax.

**`cp -r` per worker.** Considered for absolute simplicity. Rejected
because it duplicates everything (working tree *and* `.git/`),
wastes disk, and loses the ability to integrate commits between
worktrees without re-fetching.

**Merge-commit integration instead of cherry-pick.** Considered for
preserving the parallel history. Rejected because the resulting
graph is hard to follow (N merge commits per epic, each from a
short-lived worker branch), `git bisect` becomes unwieldy, and
revert semantics are messier. Cherry-pick gives a linear epic
history that maps cleanly to the panel-approval audit trail.

## See also

- `crates/brehon-git/` — git operations and recovery.
- `crates/brehon-cli/src/commands/run/setup.rs` — worktree provisioning.
- `crates/brehon-cli/src/commands/clean.rs` — `is_safe_brehon_branch`
  guard.
- `crates/brehon-cli/tests/git_tests.rs` — integration tests on temp
  repos.
- `crates/brehon-cli/tests/epic_integration_tests.rs` — full
  cherry-pick lifecycle.

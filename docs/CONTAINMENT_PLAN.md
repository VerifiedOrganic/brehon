# Containment Plan — worktrees to confined clones

**Status**: ready to execute
**Authority**: [ADR-0010](adr/0010-clone-isolation-and-confinement.md)
**Evidence**: `docs/research/containment-2026-08.md`, `docs/research/git-topology-2026-08.md`
**Gate**: `make ci` (fmt-check, check-deps, check-file-size, check, clippy, doc, test)

Every claim marked **[v]** was reproduced on the development machine
(git 2.50.1 Apple Git-155, macOS 26.6.1) during the investigation. Line numbers
were verified against the tree at the time of writing; re-check before editing.

---

## 1. The problem in one paragraph

Agents escape their worktree because a linked worktree cannot be sandboxed: its
index and objects live in `<main>/.git`, so any jail tight enough to protect the
project breaks `git commit` in the agent's own tree, and any jail loose enough
to allow `git commit` grants the shared object store — which permits a verified
chain ending in a peer's panel-approved commits becoming unrecoverable. The fix
is to give each agent a real repository of its own, seeded over `file://` so no
inode is shared, and to make the boundary a kernel property instead of a
convention. Full reasoning in ADR-0010.

---

## 2. Invariants

These are the acceptance criteria for the whole programme. Each should end up
enforced by a test, not by review.

| ID | Invariant | Enforced by |
|----|-----------|-------------|
| **I1** | No file under `<agentdir>/.git/objects` has `st_nlink > 1` | spawn-time assertion (T8) |
| **I2** | Brehon never runs the `git` binary with an agent repo as cwd / `-C` / `GIT_DIR`; inspection goes through libgit2 | T18 + clippy-visible helper boundary |
| **I3** | Brehon never pushes into an agent repo | T18; grep test for `push` with an agent path |
| **I4** | An agent process cannot write outside its clone and its declared allow-list | T13/T14 integration test |
| **I5** | The fetch-back is per-agent namespaced and non-force | T10 |
| **I6** | Spawning without a real boundary is refused unless `unsafe_marker` is set | T12/T15 |
| **I7** | Canonical's absolute path appears in no agent-readable git config | T7/T8 |

**Why I2 cannot be relaxed to "harden the flags":** `filter.<name>.clean/smudge/process`
and `merge.<name>.driver` take their driver name from in-repo `.gitattributes`,
so the set cannot be enumerated and no `-c` override kills them. They were
verified firing on `merge` and `cherry-pick` under the complete hardening
vector. libgit2 runs no hooks, filters, fsmonitor or textconv by construction,
which is why inspection must go through it.

---

## 3. Phases and tickets

Phases are ordered by dependency. **P0 is independent of the topology change and
should land first** — it contains live bugs that cost agent-time today.

### P0 — Immediate fixes, independent of topology — **complete**

T1–T6 landed hand-executed (no brehon run against brehon), `make ci` green
before each commit. Every ticket's file path needed correcting against the tree;
the line numbers were consistently right and the crate names consistently wrong.
T6 was re-aimed — see its entry.

---

#### T1 · Gemini receives no autonomy flag under worktree isolation — **done** (`72f5a71`)

**Path correction**: the site is `crates/brehon-pty/src/pty/agents/gemini.rs:277-283`,
not `crates/brehon-adapter-gemini/…` — the line numbers were right, the crate
was not. The ACP path had the identical bug via the same
`BREHON_GEMINI_ALLOW_YOLO` env contract and is fixed by the same change.
`--approval-mode yolo` is now unconditional; only the sandbox override stays
gated on the profile. **[v]** Gemini overrides yolo to `default` in an
untrusted folder rather than failing, so the `prepare_local_gemini_home`
fallback path is unchanged.

**Why**: with `worktree_isolation: true`, that gate hangs the approval flag off
`allow_privileged_mode`, so an unattended run parks at an interactive prompt
indefinitely. This is costing agent-days now and is unrelated to containment.

**Approach**: decouple the autonomy flag from `allow_privileged_mode`; select it
from the resolved permission profile the same way the other adapters do. Compare
against `crates/brehon-adapter-codex/src/codex.rs` and
`crates/brehon-adapter-claude/` for the established pattern.

**Verify**: unit test asserting the constructed argv contains the approval flag
for a `worktree_isolation: true` + non-privileged profile; then one live
single-agent Gemini task that reaches `complete` without operator input.

---

#### T2 · Denial messages leak the protected path — **done** (`7c19cec`)

Both citations were exact. `validate_mutating_path` also echoed the *resolved*
path, which was the sharper leak — a relative `../../../src/lib.rs` came back
as an absolute path under a root the model had never seen — so it now echoes
only the token the model supplied. `check_shared_root_reference` was de-pathed
too, for consistent vocabulary.

**Still open, same class, out of the ticket's stated scope**: the same
protected root is printed to the model by MCP tool errors at
`task_actions/git_ops.rs:309` and `:439`, `paths.rs:117`, and
`build_artifact_cleanup.rs:235`. `git_ops.rs:309` is reached from task
`complete`, `close`, status updates and review approval. Note also that
`claude_hook` cannot keep the path "in brehon's log only": `Commands::ClaudeHook`
falls through to `tracing_subscriber::fmt()`, whose writer is stdout, which
Claude Code captures.

**Why**: `agent_git_guard.rs:195` and `claude_hook.rs:297-302` print the
protected absolute path back to the model at the precise moment it is looking
for a way around the denial.

**Approach**: replace the path with a stable token (`the project root`). Keep
the full path in the brehon-side log only.

**Verify**: `cargo test -p brehon-cli` plus a grep assertion that the denial
string contains no `/`.

---

#### T3 · Repository hygiene — **done** (`a504b40`, fixed in `accf6a5`)

Session close prunes stale registrations via libgit2 (reusing
`WorktreeOps::cleanup_stale_metadata`, which was tested but had no production
caller) and sweeps its own empty run directories. `gc.auto` was set explicitly
to 6700 — **not** 0 — and one deliberate `git gc` ran.

Two defects in the first cut, both found by review and fixed in `accf6a5`:
the sweep recursed unbounded into live checkouts and deleted empty directories
git cannot restore (no race needed — just `auto_cleanup_worktrees: false`), and
it swept the shared `runs/` root, which could delete a concurrent session's
pre-created worktree parent and abort that run with ENOENT. It is now bounded
to the two levels the `runs/<session>/<scope>/<agent>` layout occupies and
scoped to the closing session. Wiring in `cleanup_stale_metadata` also made a
latent bug live: its `remove_dir_all` fallback deleted *locked* registrations
that `git worktree prune` deliberately keeps.

**Why**: **[v]** 76 of 81 worktree registrations in the reference project are
`prunable`; 1572 leaked worktree directories; the object store is 701 MB with
7,529 loose objects and `gc.auto` unset.

**Approach**: at session close run `git worktree prune`, then sweep empty run
directories. Filter on the `prunable` field emitted by
`git worktree list --porcelain` — **[v]** it is explicit, so no stat loop is
needed. Separately, set `gc.auto` explicitly on the project repo and run one
deliberate `git gc`.

**Ordering note**: `gc.auto=0` as blanket advice is *inverted while packfiles
are hardlinked* — gc is what de-aliases them. Only set it after T8 lands.

**Verify**: `git worktree list --porcelain | grep -c prunable` returns 0 after a
session close.

---

#### T4 · `preview_conflicts` reports false conflicts — **done** (`ed8c02a`)

**Path corrections**: the site is `crates/brehon-git/src/integration.rs:27`
(line right, crate wrong); `has_conflicts` is at `ops.rs:163`, not `:152`; the
mock is `crates/brehon-test-harness/src/mock_git.rs:317`; the trait is
`GitOperations`, not `GitPort`. The doc-comment citations at `:94` and `:105`
were exact. **[v]** The exit-code warning was right: a bad ref exits 1 with
empty stdout, exactly as a conflict exits 1. Unrelated histories exit 128.
The three harness call sites all use whole-file-replacement fixtures, which
genuinely conflict under ORT, so none needed changing.

**Why**: `crates/brehon-mcp/src/tools/task_actions/integration.rs:27` intersects
changed-file sets. **[v]** On two real project branches it reported 215
conflicting files where `git merge-tree` reported 0, in 21 ms. Its doc comments
at `:94` and `:105` falsely claim it creates a temporary worktree.

**Approach**: replace the body with
`git merge-tree --write-tree --name-only -z <a> <b>`. Guard the exit code — a
bad ref also exits 1 with empty stdout, so distinguish "conflicts" from
"invalid input". Do not use `git2::Repository::merge_commits`: libgit2's merge
is not ORT and lacks directory-rename detection, and fidelity to what `git merge`
will actually do is the entire point. Record that reason in a comment.

**Blast radius**: `has_conflicts` in `crates/brehon-git/src/ops.rs:152` is the
`GitPort` trait impl, mirrored by `mock_git.rs:317` — it is *not* dead code.
Three test-harness call sites move with it.

**Verify**: a test using a known-conflicting pair (a `<base> <base>` control
exits 0 and proves nothing) and a known-disjoint pair.

---

#### T5 · Worker protocol: merge, never rebase — **done** (`de28283`, corrected in `3fc5349`)

Landed as a HISTORY RULE in the leading containment block, with the
prohibition folded into the existing closing REMINDER rather than appended as
a new trailing step, so the ordering test keeps holding. Nothing in the tree
told agents to rebase, so this was an addition, not a contradiction to remove.

**One correction worth carrying into T6's premise**: the first cut claimed
"if your branch tip changes after approval, the reviewed commit no longer
exists and Brehon refuses to integrate your work." **[v]** That is false in
this topology — after a rebase the reviewed commit is dangling but still in
the shared object store, and `git cherry-pick -x <sha>` succeeds against it.
The real consequence is the opposite shape: integration applies exactly the
reviewed SHAs, so the rewritten work is silently dropped.

**Why**: rebase rewrites SHAs. Agent commits X → panel approves X → base
advances → agent rebases → tip is X′ → brehon integrates a commit no panel ever
reviewed, with `cherry-pick -x` provenance that looks legitimate.

**Approach**: in `crates/brehon-types/src/worker_protocol.rs`, instruct
`git merge origin/main` and forbid `git rebase` / `git commit --amend` /
`git reset` on a branch that has been submitted for review. Honour the existing
ordering discipline in that file — the containment rules are deliberately
repeated last because recency wins, and that ordering is asserted by a test.

**Verify**: extend the existing ordering test; assert the merge instruction is
present and the rebase prohibition appears in the trailing block.

---

#### T6 · Reviewed-tip-SHA gate on integration

**Why**: belt and braces for T5. If the branch tip moved after approval,
integration must refuse rather than silently ship an unreviewed commit.

**As written this ticket was redundant, and was re-aimed one level up.** Three
findings, each verified against the tree:

1. The gate it describes already ships **twice** — `action_integrate.rs:295-356`
   and `action_close.rs:600-657` compare `task.latest_commit` against the
   approved review commit via `commits_refer_to_same_oid`, demote to
   `review_ready`, and return `stale_review_approval`. Its Verify criterion is
   already met by `tool_tests.rs:9482` and `:7247`.
2. Its threat model is unreachable through `integrate`, which cherry-picks the
   frozen SHA set from the review request and never resolves a ref. A moved
   worker tip cannot ship unreviewed code. **[v]** empirically.
3. The persisted field it asks for exists: `latest_commit` *is* the worker tip
   as of the worker's last brehon call (`action_update.rs:893-902`, from
   `current_git_head()`), and `request.commit` is pinned to it at request time.
   T6 collapses to zero lines, not ten.

**The unchecked tip is the container's, not the worker's.** Container close
merges the **live** integration-branch tip into the base branch
(`action_close.rs:340` → `epic.rs` `merge --squash` / `--no-ff` / `-s ours`),
and `verify_container_branch_ready` checked only branch-exists,
worktree-is-on-it and base-is-an-ancestor — never *what* was on it. Anything
committed into the integration worktree outside `action=integrate` shipped
unreviewed.

**Approach as built** (`7cfd7c8`): nothing ever merges *into* an integration
branch — it only advances through the cherry-pick path — so `finalize_integration`
records the tip it already computes onto the container task, and
`verify_container_branch_ready` compares `rev-parse <integration_branch>`
against it and refuses on mismatch. Roughly 10 lines plus the persisted field,
needs no worker-branch name (`task["branch"]` is read in four places and
written nowhere), and is topology-independent so it survives P1.

The record is best-effort: the cherry-pick has already landed by the time it is
written, so failing the integration there would be worse than losing the
expectation. The gate is skipped when nothing was recorded, so containers
integrated before the field existed keep closing.

**Verify**: a recorded tip closes cleanly; a hand-made commit in the integration
worktree is refused naming the expected tip; an absent or blank record does not
block close.

**Related, not built** — `integration_closeout.rs:20-36` substitutes the live
`task.latest_commit` as the reviewed set when the round-request file is
unreadable (`read_round_request` swallows parse errors with `.ok()?`) and the
task is aborted or already integrated. The staleness gate at `:301` is
conditioned on `from_review_request`, so that path has no approval check at
all. Disk-corruption-gated, but it fails open.

---

### P1 — Topology: per-agent clones

Land T7 and T8 together; the fetch-back (T10) must land in the same change or
integration breaks.

---

#### T7 · `base.git` — brehon-writable, agent-read-only base mirror

**Why**: gives agents a stock `git fetch origin` sync channel without ever
naming canonical, without brehon pushing into agent-writable state, and without
exposing peers' branches.

**Approach**:

```bash
# seed, once per project
git clone --bare file://$CANONICAL $BASE      # [v] 1.16s / 22M / nlink=1
git -C $BASE remote remove origin             # canonical named nowhere agent-readable
git -C $BASE config gc.auto 0

# refresh, brehon-owned (safe: $BASE is trusted state, not agent-writable)
git -C $BASE fetch --prune file://$CANONICAL '+refs/heads/main:refs/heads/main'
date -u +%FT%TZ > $BASE/BREHON_REFRESHED_AT
```

Narrow the refspec to the configured base branch(es) so `base.git` exposes only
base. `remote remove origin` is required: **[v]** `clone --bare` writes
`remote.origin.url = <canonical>` into a 0644 file.

**Staleness is the one regression versus pointing `origin` at canonical.** If
the refresh dies at hour 6 of a 72-hour run, every agent silently develops
against a frozen base and conflict rates climb with no signal. Surface the age
of `BREHON_REFRESHED_AT` in `brehon doctor` and in run status.

**Verify**: assert `nlink=1` on `$BASE` packs; assert `git -C $BASE config
--get remote.origin.url` is empty; doctor check fails when the stamp is older
than the configured threshold.

---

#### T8 · `worktree add` → `git clone file://`

**Why**: the load-bearing change. **[v]** `git clone <path>` hardlinks packfiles
(`nlink=2`, same inode); `chmod -R u+w` followed by an in-place write inside the
agent's own directory corrupts the source. `git clone file://` yields disjoint
stores (`nlink=1`). **[v]** `git fetch` never hardlinks, so refresh cannot
reintroduce aliasing.

**Approach**: in `crates/brehon-cli/src/commands/run/factory/worktree_ops.rs`
and `crates/brehon-git/src/worktree.rs`:

```bash
git clone file://$BASE $AGENTDIR               # [v] 0.47s / 22M / nlink=1
rm -rf $AGENTDIR/.git/logs                     # reflog records "clone: from $BASE"
git -C $AGENTDIR config gc.auto 0
git -C $AGENTDIR checkout -b brehon/<worker>/<task>
```

Add the **I1 assertion at spawn**: one stat walk over
`<agentdir>/.git/objects`, refusing to spawn if any `st_nlink > 1`. This is the
regression guard for the entire hardlink class.

**Verify**: unit test on the constructed command asserting the `file://` scheme;
integration test asserting `nlink=1` post-clone and that the assertion fires on
a deliberately path-cloned fixture.

---

#### T9 · Brehon-owned agent-directory inventory

**Why**: clones are registered nowhere. `git worktree list` currently serves as
the registry for what exists, what is stale, and what recovery must clean.

**Approach**: an inventory under `.brehon/runtime/` recording agent id, clone
path, branch, base ref, and creation time. Removal becomes `rm -rf`; staleness
becomes "directory missing". Do this *before* or alongside T8 — it is the
prerequisite the clone migration rides on.

**Risk acknowledged**: brehon has already leaked 1572 worktree directories, so
handing it more lifecycle responsibility is a real bet. Mitigate by making the
inventory the same structure the ledger (T19) reads, so it is exercised
continuously rather than only at cleanup.

**Verify**: test that a killed run leaves an inventory entry whose directory is
absent, and that cleanup reconciles it.

---

#### T10 · Namespaced, non-force fetch-back

**Why**: **[v]** two agents that both named a branch `brehon/w1` — a model
copying a name out of context — force-overwrote each other in canonical, and the
loser's panel-approved commit was unreachable after gc. Worktrees prevented this
(`fatal: a branch named … already exists`); clones do not.

**Approach**:

```bash
git -C $CANONICAL fetch --no-tags --no-recurse-submodules $AGENTDIR \
    'refs/heads/brehon/*:refs/heads/brehon/<agent-id>/*'
```

No leading `+` — a rewritten history then arrives as a loud non-fast-forward
failure instead of a silent force-update, which is a free tripwire for T5/T6.
Override the per-call timeout: `DEFAULT_GIT_TIMEOUT = 60s`
(`crates/brehon-mcp/src/git_exec.rs:55`) now gates a real data transfer that did
not exist under worktrees.

**[v]** SHA identity survives the round trip, so `verification/commits.rs`,
`IntegrationOps` cherry-pick `-x`, the reviewer panel, and `is_safe_brehon_branch`
(a pure name check, and it already accepts nested names) keep working unmodified.

**Verify**: test asserting the refspec shape and absence of `+`; test that a
rewritten agent history produces a non-fast-forward error rather than a silent
update.

---

#### T11 · Post-fetch object-growth guard

**Why**: nothing bounds what the fetch-back pulls in. One agent that commits
`target/` permanently inflates canonical. Under worktrees this was a non-event
(shared store, already counted).

**Approach**: measure object count and size delta across the fetch; refuse or
quarantine above a configured threshold.

**Verify**: test with a fixture branch containing a large blob.

---

### P2 — Confinement

---

#### T12 · `confine_argv` — compile the existing `SandboxSpec`

**Why**: `brehon_types::config::SandboxSpec`
(`crates/brehon-types/src/config.rs:2310`, `SandboxBackend` at `:2199`,
`FsRootSpec` at `:2300`) is already exactly the right shape — backend,
`read_roots`, `write_roots`, `denied_roots`, network and credential class,
`env_policy`, `unsafe_marker` — fully serde'd, merged, validated and
unit-tested. The only non-test consumer
(`crates/brehon-cli/src/commands/run/agent_env.rs:56`) takes `.profile`, the
*name*, and discards `.spec`, exporting `BREHON_PERMISSION_PROFILE` for the
vendor CLI to apply to itself. **Today's confinement is an honour system.**

**Write the compiler; design nothing.** No trait — `SandboxBackend` is already
an enum with one implementation per platform, so match on it. An interface with
one implementation per arm is not an abstraction.

```rust
// crates/brehon-mux/src/pane/confine.rs
pub struct ConfineCtx<'a> { pub work: &'a Path, pub runtime_dir: &'a Path }

/// Ok(vec![]) ONLY when spec.unsafe_marker. Err = fail closed, refuse to spawn.
pub fn confine_argv(spec: &SandboxSpec, ctx: &ConfineCtx) -> Result<Vec<String>>;

pub fn probe() -> ConfineSupport;
pub struct ConfineSupport {
    pub backend: Option<SandboxBackend>,
    pub detail: String,
    pub unenforceable: Vec<&'static str>,  // shown BEFORE a run, not after an incident
}
```

**Compiler invariants — enforce, do not discover:**

1. **`denied_roots` must be disjoint from `write_roots`.** Landlock has no deny
   rules, only allow-grants over a default-deny handled set, so a nested deny is
   *unexpressible* there. Error at compile time rather than silently
   under-enforcing on one platform.
2. **Emit order `read_roots → write_roots → denied_roots`.** SBPL is
   last-match-wins and deny-before-allow is silently defeated. `SandboxSpec`
   happens to declare the fields in that order — that is luck. Golden-test the
   emitted text.
3. **Canonicalize every path** before templating. `/tmp` vs `/private/tmp`
   renders the profile inert; it fails closed but presents as a broken agent.
   Three separate investigation lanes hit this.
4. **Reject a `write_root` that is an ancestor** of any read-only root, of
   canonical, of `base.git`, of the brehon runtime dir, or of the emitted policy
   file. A lane destroyed its own test canonical with a blanket `/private/tmp`
   grant.
5. **Grant per-agent `TMPDIR` under the work root, never `/tmp`.** **[v]**
   `git init/add/commit` needs no tmp grant at all. Note macOS `mktemp -t` uses
   `_CS_DARWIN_USER_TEMP_DIR` and ignores the override, so nothing brehon owns
   may live in the shared dir.

**Argv prefix, not `pre_exec`**: `Pty::spawn`
(`crates/brehon-pty/src/pty/core.rs:104`) goes through `portable_pty::CommandBuilder`,
which exposes no `pre_exec` and consumes it internally for setsid/TTY. Compile
to a prefix — the shape `sandbox-exec` and `bwrap` already have. `sandbox-exec`
execs in place, so setpgid, `kill_on_drop` and reaper logic are untouched.
`crates/brehon-adapter-sdk/src/process.rs` does expose `pre_exec` and stays
available as an escape hatch; do not fork the design over it.

**Verify**: `confine_argv` is pure — golden-test the emitted SBPL and argv
(ordering, nested-deny error, realpath). Plus **one** integration test that
spawns through the real prefix and asserts an out-of-root write fails. No
framework, no fixtures.

---

#### T13 · Seatbelt backend (macOS)

**Approach**: `sandbox-exec -f <profile.sb> -D OWN=<realpath> …`. Use `-D`
parameters, never string interpolation — injection-safe under a policy-closing
payload, and a missing parameter is a hard startup error rather than a silent
hole.

Beyond the write allow-list, three rules each correspond to a reproduced
failure:

```scheme
(deny signal)
(allow signal (target self) (target children))
```

**[v]** A confined agent killed a process outside its sandbox. `pkill -f node`
is ordinary dev-server cleanup and it ends a 72-hour run. Do **not** use
`(target pgrp)` — that reopens the hole whenever the victim shares a process
group.

```scheme
(deny network-outbound (regex #"^/private/tmp/tmux-"))
```
`(allow default)` leaves AF_UNIX `connect()` open, and a tmux server is
routinely running on the development machine; `tmux send-keys` executes in an
unsandboxed pane. Deny the docker socket on the same principle — if a lane
genuinely needs docker, confinement is decorative for that lane and should be
labelled so.

**Grant cache leaves, never parents.** Granting `~/.cargo` is a full escape:
`~/.cargo/bin` is on the operator's PATH and was verified receiving an
executable that the operator's own shell then resolved; `~/.rustup/toolchains/*/bin`
holds `cargo` and `clippy-driver`; `~/.claude/settings.json` has a live `hooks`
key that Claude Code executes. Grant `~/.cargo/registry`, `~/.cargo/git`,
`~/.rustup/downloads`, `~/.rustup/tmp`, `~/.npm/_cacache`. Redirect
`CLAUDE_CONFIG_DIR` and `CODEX_HOME` per-agent into the work root — one env var
each, and it removes the class.

Grant exactly `<project_root>/.brehon` and nothing else inside the project
repository: `BREHON_ROOT` points there and the MCP server is a child of the
agent CLI, so it inherits the profile. This is the one deliberate hole; it is
coordination state, not source or history. Log it.

**Known and accepted**: setuid binaries cannot exec under any profile, so `ps`,
`top`, `sudo`, `su` and `traceroute` fail while `pgrep`, `pkill`, `lsof` and
`netstat` work. Agents shell out to `ps` constantly — document the substitutes
in the worker protocol.

---

#### T14 · Landlock backend (Linux)

**Approach**: `<current_exe> confine --policy <file> --` using the `landlock`
crate (0.4.7, MSRV 1.71, no root, no daemon). Unprivileged via
`PR_SET_NO_NEW_PRIVS`, inherited across `execve`, irreversible from inside — a
genuine peer of Seatbelt on the properties that matter.

**Floor at ABI ≥ 3, error below it.** Not `CompatLevel::BestEffort`, which
silently degrades past the `truncate` gap. Build the handled set with
`AccessFs::from_all(abi)` so `TRUNCATE` cannot be hand-dropped.

**Implementation trap**: `restrict_self()` is per-thread by default and
brehon's entry point is `#[tokio::main]`
(`crates/brehon-cli/src/main.rs:376`). Re-entering `<current_exe> confine` with
worker threads already alive confines one thread of a process that then execs
the agent — writes succeed and a golden test on argv text still passes.
**Intercept `confine` in a non-async `main()` on raw `std::env::args()` before
the runtime is built**, and call `all_threads()` as belt-and-braces.

**Note the asymmetry**: leaf-only cache grants are *mandatory* here, because
"grant `~/.cargo`, deny `~/.cargo/bin`" is exactly the nested-deny Landlock
cannot represent — otherwise macOS and Linux silently disagree on the rule that
matters most.

---

#### T15 · Make confinement structurally unskippable

**Why**: `apply_grok_acp_hardening` has two call sites; a third spawn path would
silently miss confinement.

**Approach**: apply inside `Pty::spawn` with a **required, non-`Option`**
`confine: SandboxSpec` field on `PtyConfig`
(`crates/brehon-pty/src/pty/config.rs`), so the compiler forces every present
and future call site to choose. `brehon-pty` already depends on `brehon-types`.

**That guarantee dies the moment someone writes `..Default::default()`**, and
`impl Default for PtyConfig` exists at `pty/config.rs:68`, used only by tests
and a doc comment. Delete it if the invariant is to be real. The operator's own
terminal (`pane/terminal.rs:76`) then passes an explicit `unsafe_marker: true` —
visible and deliberate.

**Fail-closed rule**: `confine_argv` returns `Ok(vec![])` only when
`unsafe_marker` is set. Any other inability to build a boundary is `Err`, and
`Err` refuses the spawn, loudly, naming the reason.

---

#### T16 · Doctor: confinement probe

**Approach**: wire `probe()` into `crates/brehon-doctor/src/checkers`. Report
the active backend and the `unenforceable` list **before** a run starts. Add the
`BREHON_REFRESHED_AT` staleness check from T7 and a cross-filesystem
`worktree_root` check (with `file://` this is now a cost warning, not a hazard).

---

### P3 — Integration hardening

---

#### T17 · Tier A/B git flags, and an absolute `git` path

**Why**: threading one global vector through `run_git` **breaks brehon** —
**[v]** `-c protocol.allow=never` makes `ls-remote https://…` fail
`transport 'https' not allowed`, and canonical has a real GitHub origin.
Separately, brehon spawns `Command::new("git")`
(`crates/brehon-mcp/src/git_exec.rs:66`, `:176`, `:186`, plus 110 raw sites in
`brehon-cli`), which resolves through **brehon's** PATH including `~/.cargo/bin`
— an agent that drops a `git` there gets it executed by brehon.

**Tier A — always**, in `run_git` (`git_exec.rs:65`):
```
--no-pager  -c core.fsmonitor=false  -c core.alternateRefsCommand=  -c gc.auto=0
```
plus the existing non-interactive env, plus **git resolved to an absolute path
once at startup**.

**Tier B — only on calls touching an untrusted repo**: Tier A plus
`-c core.hooksPath=<brehon-owned empty dir>` (**not** `/dev/null`, which is
`NUL` on Windows, and **never globally** — that disables
`BREHON_PROTECTED_BRANCH_HOOKS` including the `reference-transaction` guard on
the very fetch that writes agent refs into canonical), `--no-ext-diff`,
`--no-textconv`, `-c diff.external=`, `-c core.sshCommand=`, `-c core.askPass=`,
`-c credential.helper=`, `-c gpg.program=false`, `-c core.editor=false`,
`-c sequence.editor=false`, `-c fetch.fsckObjects=true`,
`-c transfer.fsckObjects=true`, and `GIT_CONFIG_NOSYSTEM=1` /
`GIT_CONFIG_GLOBAL=<null>`.

**Tier B is defense-in-depth, not a boundary.** `filter.<name>.*` and
`merge.<name>.driver` walk through all of it — hence T18.

---

#### T18 · Inspect via libgit2, mutate only in canonical

**Why**: the achievable form of I2/I3. libgit2 is immune by construction and
brehon already uses it here (`crates/brehon-git/src/worktree.rs:179`, `:469`,
`:598`; the `*_with_git2` helpers in `factory/worktree_ops.rs`) — which is
exactly why `worktree_state_check`, `is_worktree_clean` and `archive_worktree`
survive the topology change untouched. Generalise that from coincidence into
policy.

**Approach**:
- Re-point `git_patch_id` (`crates/brehon-git/src/git_ops.rs:606-608`) and the
  `rev-list` at `:649` at canonical, after the fetch-back.
- Audit and migrate the `epic.rs` sites that run `merge`, `cherry-pick`,
  `commit` and `reset --hard` with an agent directory as cwd (`:1213`, `:1231`,
  `:1267`, `:1279`, `:1316`, `:1461`, `:1462`). These are the un-hardenable
  ones; they must operate in canonical.
- `git_path_has_tracked_files` (`worktree.rs:804`, `:825`) shells out and fires
  `core.fsmonitor` — move it to libgit2.

**This is the real work of the plan.** It is an architectural change to the
integration flow, not a flag prepend.

**Verify**: a test fixture agent repo with a hostile `.gitattributes` selecting
a `filter.*.process` driver; assert no driver executes across the full
integration path.

---

### P4 — Deletion

Do not start P4 until P2 is live. Deleting the advisory guards while the shared
`.git` is still reachable removes the only thing between a careless `git gc` and
a peer's unmerged commits.

- **T20** — delete `crates/brehon-cli/src/commands/run/setup/agent_git_guard.rs`
  (438 lines **[v]**) and its PATH plumbing. Advisory only; bypassed by
  `/usr/bin/git`.
- **T21** — delete the bypass-lease machinery
  (`run_git_allow_protected_branch_commit`, `ProtectedBranchBypassLease`,
  `create_protected_branch_bypass_lease`, `git_exec.rs:72-140`) once
  integration happens in canonical.
- **T22** — prune `worktree.rs` registration bookkeeping
  (`find_worktree_name_by_path`, `scan_worktrees_dir_for_path`,
  `reconstruct_worktree_name`, stale-metadata recursion). A clone has no parent
  registration. **Keep the libgit2 functions** — they are immune by
  construction.

Expected net across the programme: roughly **−1000 / +300** lines.

---

### Optional — T19 · Ledger and overlap matrix

Not required for containment; recorded because it shares the T9 inventory and
answers "who is about to conflict with whom".

Per-tick: snapshot each agent's WIP without mutating its index
(`cp .git/index` to a temp path, then `GIT_INDEX_FILE=… git add -A` +
`write-tree`; skip `commit-tree` when the tree is unchanged, or you write one
object per agent per tick forever), fetch into canonical, then compute the
overlap matrix with `git merge-tree --write-tree --name-only -z` (~20 ms/pair).

Feed the matrix to a **merge queue**, not `DependencyGraph`: conflict is
symmetric, so the resulting 2-cycles error out of `topological_order`, and
`get_ready_tasks` would gate task *start* on dependency *completion* — the wrong
phase, on work already in flight. Merge the largest disjoint batch and bisect on
CI failure.

---

## 4. Non-goals

Explicitly not built. Each was evaluated and rejected on evidence.

- The `ext::` broker, `GIT_NAMESPACE`, a local HTTP server, `sshd`, `git daemon`.
- Per-agent tokens or keys — all agents share one uid with reads open, so any
  credential is peer-readable. Transport-level identity here is theatre.
- gitoxide or libgit2 as a wire-protocol server (gitoxide's `crate-status.md`
  leaves send-pack/receive-pack unchecked; libgit2 #5605 open since 2020).
- A new sandbox policy type or a confinement trait — `SandboxSpec` exists and
  `SandboxBackend` is already an enum.
- `extensions.preciousObjects`; `chmod -R a-w` or `chflags uchg` on any repo
  (owner chmods back at one uid, and `uchg` produces an exit-0 half-applied
  `git checkout`).
- More `PreToolUse` command-inspection hooks — `claude_hook.rs` is 768 lines and
  escapes continued; that experiment has run.
- Launch-refusal for CLIs lacking hooks — it trades away the cross-vendor panel,
  which is the product. Run them under confinement instead.
- Inverting the git shim to an allow-list — moot after P2, and
  `git diff --output=/abs/path` defeats it anyway.
- Containers, VMs, or per-agent UNIX uid **for now**. Per-agent uid is the only
  complete answer and the only credible Windows path; build it when any of:
  an agent is no longer same-trust with the operator; vendor credentials must be
  isolated per agent; or the fleet goes multi-machine.

---

## 5. What remains unenforceable

Record these where the operator sees them, not in a postmortem.

| Gap | Platform | Disposition |
|-----|----------|-------------|
| **Reads** — the agent can always *learn* canonical's path (`ls ~/workspace`, `.mcp.json`, tracked docs) | all | Accepted. Denying reads breaks rustc, cargo, node and every vendor CLI. P1 removes the git-sourced paths; P2 makes the knowledge inert. |
| `filter.<name>.*`, `merge.<name>.driver` | all | Unkillable by flags. Addressed structurally by T18, not by hardening. |
| `chmod` / `chown` / `utime` / `setxattr` | Linux | Landlock never restricts these. This is *why* T8's `file://` matters more than the sandbox. |
| `truncate(2)` below ABI v3 (kernel 6.2) | Linux | Hard floor: error rather than degrade. |
| Signal scoping below ABI v6 (kernel 6.12) | Linux | The T13 signal rule has no Landlock equivalent on most kernels in service. |
| Unix-socket connect below ABI v9 (Linux 7.1) | Linux | No shipping LTS. Also the reason the broker option was rejected. |
| setuid binaries (`ps`, `top`, `sudo`) cannot exec | macOS | Not fixable in-profile. Document `pgrep`/`procs` substitutes. |
| Any path confinement | Windows | `Err`. Error message names WSL2 and containers. |
| `docker run -v` writes as the Docker VM | all | Deny the socket, or accept that confinement is decorative for that lane. |
| The `.brehon` grant is a cross-agent interference surface | all | Coordination state only. Accept and log. |
| `sandbox-exec` is man-page-deprecated | macOS | No runtime warning; Chrome and Codex depend on it. Revisit if a major release removes it. |

---

## 6. Kickoff

**Do not run brehon against brehon to execute P0 or P1.** The defect being
fixed is the one that would corrupt the repository mid-run: today every Codex
agent has `<project>/.git/hooks/` in its writable roots, and the shared object
store makes a careless `git gc` unrecoverable for a peer. Hand-execute P0, land
P1 and P2, then dogfood P3 and P4 under the new boundary.

**Order**: T1–T6 in any order (independent). Then T9 → T7 → T8 → T10 → T11 as
one reviewed change. Then T12 → T13/T14 → T15 → T16. Then T17 → T18. Then P4.

**Definition of done for each ticket**: `make ci` green, the ticket's own
verification passing, and — for T8, T10, T13, T14, T15 — the corresponding
invariant from §2 covered by a test that fails when the mechanism is removed.

**First session, concretely**: T3 (hygiene — reclaims the leaked worktrees and
the loose-object bloat, zero risk), then T1 (the Gemini flag, which is costing
agent-time every unattended run), then T2. Those three are independent, small and
verifiable, and they leave the tree in a better state before the topology change
begins.

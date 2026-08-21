<!-- Generated 2026-08-20 by a 9-agent adversarial workflow (4 investigation lanes
     + 4 refuters + synthesis, Opus 5, ~1.08M tokens). Claims marked [V] were
     reproduced on this machine. Target: /Users/recursive/workspace/brehon -->

# BREHON CONTAINMENT — RECOMMENDATION

*All claims below marked **[V]** were reproduced on this machine during this pass; unmarked claims are cited from the lanes. Scratch artifacts removed.*

---

## 1. ROOT CAUSE

**There are two root causes and they are different in kind.**

### 1a. THE STRUCTURAL ONE — no guard for this class can exist under the current layout

A linked worktree is not a self-contained repository. Its index, refs, logs and objects live in `<main>/.git`. Therefore:

- Any write-jail tight enough to protect the operator's repo **breaks `git commit` in the agent's own worktree** — `fatal: Unable to create '<main>/.git/worktrees/<n>/index.lock': Operation not permitted` (reproduced independently by two lanes).
- Any jail loose enough to permit `git commit` **must grant the shared object store**, and that grant is fatal. Verified chain from inside such a sandbox, cwd = the attacker's own worktree: `rm -rf <main>/.git/worktrees/<sibling>` → sibling is `fatal: not a git repository` mid-run → `git branch -D <sibling branch>` → `git reflog expire --all && git gc --prune=now` → sibling's unmerged commit is **unrecoverable** (`cat-file` fails). `.git/logs` is inside the same grant, so "visible in the reflog" is not a mitigation.

brehon has already made this trade, in production, explicitly:

```
crates/brehon-adapter-codex/src/codex.rs:1398  fn git_writable_roots_for_worktree(...)
    → pushes the resolved gitdir  AND  the resolved commondir  as writable roots
    → consumed at codex.rs:1317 into the per-turn Seatbelt workspaceWrite policy
```

**[V]** Confirmed in the tree. So today, under brehon's only real OS sandbox, `/Users/recursive/workspace/pantheon/.git/hooks/post-commit` and `.git/config` are *whitelisted* for every Codex agent. The headline RCE is not unblocked — it is authorized.

There is no hole to patch here. The primitive is wrong.

### 1b. THE GUARD-HAS-A-HOLE ONES — real, but second-order

| Hole | Evidence |
|---|---|
| `agent_git_guard.rs:191` triggers only on `"$repo_root" == "$project_root"` **[V]** — sibling worktrees are outside its scope entirely | `crates/brehon-cli/src/commands/run/setup/agent_git_guard.rs:191` |
| Deny-list at `:193` is `add\|am\|apply\|checkout\|cherry-pick\|clean\|commit\|merge\|mv\|pull\|rebase\|reset\|restore\|revert\|rm\|stash\|switch\|update-index` **[V]** — omits `worktree`, `branch`, `update-ref`, `symbolic-ref`, `config`, `gc`, `reflog`, `push`, `tag`, `filter-branch`, `init`, `clone`, `sparse-checkout` | `agent_git_guard.rs:193` |
| `--git-dir=`/`--work-tree=` consumed at `:140-145` **[V]** but never fed to the `rev-parse --show-toplevel` probe at `:162` → command-line form bypasses; env-var form is caught | `agent_git_guard.rs:140-145,162` |
| PATH-prepend only → `/usr/bin/git`, `bash -lc`, `env -i`, libgit2 all bypass | `agent_path.rs:30-40` |
| Enforcement asymmetry: Claude gets `claude_hook.rs` (768 lines **[V]**), Codex gets its own Seatbelt, OpenCode gets a path jail; **Gemini/Kimi/Copilot/Agy/Junie get the prompt and the shim** | `setup.rs:1600`; `copilot.rs:84-85`; `kimi.rs:841`; `agy.rs:121-122` |
| Detection is one check at run start and one at shutdown | `setup.rs:2294-2325` called once at `mod.rs:3125` |
| `SandboxSpec.read_roots/write_roots/denied_roots` has zero non-test consumers — profile names are labels | `config.rs:2310-2332`; `agent_env.rs:56-81` |

### 1c. The correction to the priors

The prior audit's root cause ("`--dangerously-skip-permissions` at `brehon-adapter-claude/src/lib.rs:80`") is **wrong twice**: that line is inside `#[cfg(test)] fn from_params`, and the default `sandbox_profile: OsDefault` does not pass that flag — production passes `--permission-mode dontAsk --allowedTools ...,Write,Edit,MultiEdit,...` (`pty/agents/claude.rs:48-56,343-355`). Same outcome, different lever. Removing the flag fixes nothing.

And `claude_hook.rs` existing at 768 lines **while you still report escapes** is the strongest evidence in this whole report that command-string inspection is not a boundary. It is not a thing to add; it is the experiment that already ran.

---

## 2. ESCAPE VECTOR TABLE

Ranked by likelihood a careless model hits it. "L1/L2/L3" = layers in §3.

| # | Vector | Today | Closed by |
|---|---|---|---|
| 1 | Non-Claude agent (Gemini/Kimi/Copilot/Agy/Junie) writes any absolute path — own Write tool, `echo >`, `python3 -c open()`, `cp` | **open** | **L1** |
| 2 | Claude `Write`/`Edit` to an absolute path — in-process, permission system off | hook-blocked (token matcher) | **L1** |
| 3 | Bash write forms the hook's matcher doesn't cover: `dd of=`, `tar -C`, `unzip -d`, `rsync`, `ln -s`, `xargs`, `$VAR` paths, heredoc-then-run | **open** | **L1** |
| 4 | `git worktree list` / `cat .git` enumerate main + every sibling; `ls ..` does it without git | **open** (a guard unit test *asserts* it's allowed, `agent_git_guard.rs:303-320`) | **L0** (partially — `ls ..` needs per-agent parent dir) |
| 5 | Recon: `BREHON_PROJECT_ROOT` in agent env, in `<worktree>/.mcp.json` **[V]** (5 hits in pantheon's live file), in `.claude/settings.local.json`, in 6 *tracked* pantheon docs | **open** | nothing — accept it; L1 makes it inert |
| 6 | Writes into a sibling agent's tree (git or plain file I/O) | **open** | **L1** + **L0** |
| 7 | `git -C <main> worktree remove --force <sibling>` — `worktree` not in deny-list | **open** | **L0** (no sibling registry exists) |
| 8 | `git -C <main> config core.hooksPath /tmp/evil` — silently no-ops brehon's only PATH-independent layer | **open** | **L1** **[V]** denied |
| 9 | Agent writes `<main>/.git/hooks/post-commit` → RCE in operator's checkout on their next commit | **open, and whitelisted for Codex** (`codex.rs:1398`) | **L1** **[V]** denied |
| 10 | `reflog expire --all && gc --prune=now` in shared store → sibling's unmerged commits unrecoverable | **open** | **L0** (separate object stores) |
| 11 | `update-ref refs/heads/main`, `symbolic-ref HEAD`, `branch -D <sibling>` | **open** | **L0** + **L1** |
| 12 | `/usr/bin/git`, `bash -lc`, `env -i`, libgit2/pygit2/nodegit | **open** | **L1** (kernel doesn't care which binary) |
| 13 | `--git-dir=`/`--work-tree=` command-line bypass of the shim | **open** | **L1** (shim deleted) |
| 14 | Claude `Read`/`Grep`/`Glob`/`LS` unrestricted — the recon step | **open** | **not closed, deliberately** |
| 15 | `tmux send-keys` / `tmux new-session` into an unsandboxed pane on the live `gauntlet` server | **open, and survives a naive Seatbelt profile** | **L2** **[V]** `(deny network-outbound (regex #"^/private/tmp/tmux-"))` |
| 16 | `docker run -v /host:/host` — the write is done by the Docker VM; daemon is running on this box | **open, survives Seatbelt** | **L2** (deny docker socket) — and if you need docker in-agent, L1 is decorative for that lane |
| 17 | Denial messages print the protected path back to the model at the moment it's hunting a workaround | **open** | one-line fix, `agent_git_guard.rs:195`, `claude_hook.rs:297-302` |
| 18 | Inherited fd across exec (Seatbelt checks `open()`, not `write()`) | **already closed** — portable-pty 0.9 `close_random_fds()` in `pre_exec`, all spawns route through `pty/core.rs:121` | keep as a test, not new code |
| 19 | `sshd` / other launchd daemons with write-capable local APIs | low (Remote Login off) | L2 if enabled |

---

## 3. RECOMMENDED ARCHITECTURE

Three layers. One is load-bearing. One is a *prerequisite that is not itself a boundary*. One is detection.

### L0 — PREREQUISITE (not a boundary): replace `worktree add` with `git clone`

**What.** Per agent: `git clone <main> <agentdir>` — plain local clone, **not** `--shared`, **not** `--reference`, no alternates. Then `git -C <agentdir> remote remove origin` and `git -C <agentdir> checkout -b brehon/<worker>/<task>`.

**Why it is the prerequisite.** It moves the agent's entire git universe inside one directory, so the Seatbelt rule becomes *"deny all writes except this one subtree"* — which is simultaneously (a) tight enough to protect everything and (b) loose enough that `git commit` works. That combination is impossible with a linked worktree.

**Measured, on real pantheon [V]:**

| | clone (with checkout) | `worktree add --detach` |
|---|---|---|
| time | 2.45 s | 0.29 s |
| working tree | 15 M | 15 M |
| `.git` incremental (hardlink-aware) | **156 K** | — |

Objects are **hardlinked**, not copied and not alternate-referenced. **[V]** I deleted a branch in the source, `reflog expire --expire=now --all`, `gc --prune=now` → source lost the object, **borrower kept it, `fsck` clean, checkout and commit fine.** This is the fact both lane 3 and its refuter missed: `--shared` has the corruption hazard, a plain local clone does not. `extensions.preciousObjects` is therefore unnecessary — drop it, and with it the `git repack -ad`/`git prune` fatals and the silent no-op `git gc`.

**Direction rule — seed by clone, integrate by fetch, NEVER push into an agent repo. [V]** I planted `pre-receive`/`post-receive`/`update`/`post-update`, `uploadpack.packObjectsHook`, `core.alternateRefsCommand`, `core.pager`, `core.fsmonitor` in an agent clone. `git fetch` from it: **zero** hostile execution. `git push` into it: **5 hooks + `core.alternateRefsCommand` fired as the operator.**

**Blocks:** vectors 7, 10, 11 structurally (no shared object store, no sibling registry, no shared hooks dir), and most of 4.
**Does not block:** anything at the filesystem level. The agent still has `ls ..`, still has the path in `.mcp.json`. **This is not containment.** It is the layout change that makes containment deployable.

**Cost against actual code — and the lazy variant that keeps it small.** The naive version breaks the world: `Git2Operations::open` uses `Repository::discover` (`ops.rs:32`), review preflight resolves agent SHAs in the main repo (`verification/commits.rs:23-60`), integration cherry-picks local refs, `clean.rs` scans local branch names. Refuter counted 425 git-subprocess call sites.

**Don't port any of it.** Add one ingest step to the ledger tick:

```
git -C <main> fetch <agentdir> '+refs/heads/brehon/*:refs/heads/brehon/*'
```

**[V]** SHA identity is preserved across the round trip. After the fetch, `refs/heads/brehon/<worker>/<task>` is a local ref in the operator's repo *exactly as it is today* — so `verification/commits.rs`, `IntegrationOps` cherry-pick `-x`, `is_safe_brehon_branch` (pure name check, `clean.rs:92`), the reviewer panel, and lane 4's "386 refs already exist" diff substrate all keep working **unmodified**. Real diff: `worktree_ops.rs` swaps `worktree add`→`clone`, `recovery.rs` loses its commondir logic, plus one fetch line. Not 425 sites.

**Residual risk after refutation:** brehon loses `git worktree list` as the registry of what agent directories exist and must own that list in `.brehon`. Given 1572 leaked worktree dirs and **[V]** 76-of-81 `prunable` entries in pantheon today, brehon is already bad at this — the change makes an existing bug more visible, not more likely. Hardlinks also pin inodes, so the operator's `gc` reclaims nothing while clones live.

### L1 — LOAD-BEARING: Seatbelt write-jail on every agent spawn

**What.** Wrap every agent PTY: `/usr/bin/sandbox-exec -f <profile.sb> -D AGENT=<canonicalized clone> -D TD=<canonicalized TMPDIR> ... <agent-cli> <args>`.

**Verified end to end on the clone layout [V]** — this exact profile shape, one run:

| probe | result |
|---|---|
| `git add && git commit` inside the clone | **rc=0** ✅ (impossible under worktrees) |
| `echo x > <main>/ESCAPE.txt` | `Operation not permitted` |
| `echo x > <main>/.git/hooks/post-commit` | `Operation not permitted` |
| `/usr/bin/git -C <main> config brehon.pwned true` | `could not lock config file` |
| `python3 -c "os.unlink('<main>/f')"` | `PermissionError: [Errno 1]` |
| `ls /nope 2>/dev/null` | works |
| `tmux -S /private/tmp/tmux-501/default list-sessions` | `Operation not permitted` |
| `curl https://api.anthropic.com/...` | `http=405` (TCP untouched) |

**Non-negotiable profile rules** (each one is a reproduced failure, not a style preference):

1. **`std::fs::canonicalize` every `-D` path.** Seatbelt matches realpath. `(subpath "/tmp/x")` silently denies. `orchestration.worktree_root` (`config.rs:1308`) is an un-canonicalized operator string and pantheon's real trees are under `/private/tmp` — the naive implementation locks agents out of their own clone.
2. **Use `-D`, never `format!()` into the profile.** Injection-safe (tested with a policy-closing payload); a missing param is a hard startup error, i.e. fail-closed on a typo.
3. **`(allow default)` + `(deny file-write*)` + allow-list.** Reads stay open. Network stays open.
4. **Allow `/dev/null`, `/dev/zero`, `/dev/dtracehelper`, `(subpath "/dev/fd")`, `(regex #"^/dev/tty")`** or every `2>/dev/null` in every build script dies deep inside a vendor binary.
5. **Allow the toolchain and vendor state dirs the workflow actually needs:** `~/.cargo` **whole** (not just `registry` — `.package-cache` is the global lock and `git/` is where brehon's own `panesmith` dep lands), `~/.rustup`, `~/.gitconfig`, `~/Library/Caches`, `~/.claude` + `~/.claude.json*` + `~/.local/{state,share}/claude` (native install here), and `~/.codex ~/.gemini ~/.copilot ~/.kimi ~/.config/opencode ~/.junie` — or OAuth refresh dies for every vendor but Claude on day two of an unattended run.
6. **Nest-deny the self-escalation paths** (last match wins, verified): `~/.claude/{settings.json,hooks,plugins,skills,agents}`, `<clone>/.claude`, `<clone>/.mcp.json` — otherwise the agent writes a hook that brehon executes *outside* the sandbox next launch.
7. **Grant exactly `<project_root>/.brehon` and nothing else in the operator's repo.** `BREHON_ROOT=<project_root>/.brehon` **[V]** and the MCP server is a *child of the agent CLI*, so it inherits the profile; brehon-mcp has 734 write sites under it. Without this grant the control plane dies on spawn. This is the one deliberate hole and it is not git state.

**Blocks:** vectors 1, 2, 3, 6, 8, 9, 12, 13 — every filesystem write escape, from every CLI, every language, every binary, absolute paths included.
**Does not block:** reads (deliberate — vector 14), network egress and exfiltration, out-of-process helpers not explicitly denied, or anything the model does *inside* its own clone.

**Cost.** ~40 lines of profile + one `Command::new` wrap at the PTY spawn choke point (`brehon-pty` + `brehon-adapter-sdk/src/process.rs:116`). Filesystem overhead measured at zero on synthetic stat/open benchmarks; **nobody has timed a real `cargo build` under the profile — do that before shipping, it's 20 minutes and it's the number that matters.** Budget a day of allow-list whack-a-mole per vendor CLI, and ship the profile as an editable file per profile name with one `extra_write_roots` knob.

**Residual risk that survived adversarial review:**
- **`docker run -v`** writes as the Docker VM. Daemon is live on this box. Deny the socket or accept that this layer is decorative for any lane that uses docker.
- **`/bin/ps` and `/usr/bin/top` are setuid-root on macOS 26** — the same block that stops `sudo` returns EPERM for `ps aux`. Not fixable in-profile. Agents run `ps` constantly to manage dev servers. Ship `pgrep`/`procs` guidance in the worker protocol.
- `sandbox-exec` is man-page-deprecated (no runtime warning on 26.6.1). Chrome and Codex depend on it.
- **Nested sandboxes fail** (`sandbox_apply: Operation not permitted`). Codex drives its own Seatbelt per turn — either use its native policy or give it `danger-full-access` *inside* the outer wrapper. Do not double-wrap.
- The `.brehon` grant is a cross-agent interference surface. Coordination state, not source or history. Accept it, log it.

### L2 — part of L1's profile, called out because it's the one everyone gets wrong

`(deny network-outbound (regex #"^/private/tmp/tmux-"))` and the same for the docker socket. `(allow default)` leaves AF_UNIX `connect()` wide open, and the escape is not theoretical: your own gauntlet/challenge/swarm skills keep a tmux server running, and `tmux send-keys` executes in an unsandboxed pane. **[V]** the deny works and leaves TCP untouched. The principled version is deny-unix-connect-by-default with an allow-list for the Postgres socket; that contradicts "unix sockets work unchanged" but it's the only version that closes the class.

### L3 — DEFENSE IN DEPTH: detection

**Containment poll, every 60 s** (measured free: `git status --porcelain` on 143,749 files = **0.017 s**):

```
git -C <main> rev-parse HEAD                    # NOT stat .git/logs/HEAD — see §7
git -C <main> status --porcelain --ignored=no
git -C <main> config --get core.hooksPath       # catches the silent kill of the hook layer
```

Diff against a **rolling** baseline, not absolute cleanliness — you edit main during runs. Under L0+L1 this should never fire; when it does, it means a lane launched unsandboxed. That is the alarm you actually want.

**Seatbelt denial stream** — free attribution telemetry: `log stream --predicate 'process == "kernel" AND eventMessage CONTAINS "Sandbox: "'` yields `Sandbox: bash(72421) deny(1) file-write-data <path>`. Verified no coalescing under a 300-denial burst. **Budget a live pid-tree mapper, not a line parser** — the line carries process name + pid only, and PIDs recycle over multi-day runs.

**Keep `claude_hook.rs`. Do not extend it as a control.** It is now a *friendly error message generator* — a model that gets a legible "that's outside your workspace" recovers better than one that gets bare EPERM. Wire `brehon tool-guard` into Gemini `BeforeTool` and Copilot hooks (verified identical exit-2 contract) for the same reason. **Do not ship the launch-refusal gate** — refusing to launch hookless lanes trades away the cross-vendor panel, which is the product. Run them under L1 instead.

---

## 4. WORKTREE vs CLONE vs BARE-MAIN — DECISION

**Per-agent plain local clone (hardlinked, no alternates), seeded by clone, integrated by fetch.**

- **Worktrees: rejected.** Not because they're bad, but because §1a — a linked worktree cannot be sandboxed. Every jail either breaks `git commit` or authorizes the destruction chain. There is no third option.
- **Bare main: rejected.** It removes the main *checkout* as a write target and nothing else. Siblings still enumerate, the common dir is still shared, the hooks are still shared, `gc --prune=now` still destroys peers. It solves the least valuable third of the problem and costs you your daily checkout.
- **`clone --shared` / alternates: rejected.** Inherits the source-`gc` corruption hazard, which then forces `extensions.preciousObjects`, which makes `git repack -ad` and `git prune` fatal and `git gc` a **silent** no-op on a 701 MB object store with `gc.auto` unset **[V]**. All of that to save 156 K.

**Strongest argument against the recommendation:** you lose git's own worktree registry. `git worktree list` is currently brehon's source of truth for what agent directories exist, what's stale, and what recovery has to clean up — `recovery.rs`, `clean.rs`, and the prunable-detection all lean on it. Clones are registered nowhere; brehon must own that inventory in `.brehon` and GC it itself. Given that brehon has already leaked 1572 worktree directories and 76-of-81 registrations, handing it *more* lifecycle responsibility is a real bet. Mitigation: the ledger (§5) is that inventory — build it first and the clone move rides on top of it.

Secondary counter, weaker: +2.2 s per agent spawn and hardlink-pinned inodes in the operator's object store. Both noise.

---

## 5. SYNC + DIFF DESIGN

One tick does everything. 60 s timer, or on epic-branch movement.

### Ledger row (one per live agent, in `.brehon/runtime/ledger.json`)

```json
{
  "agent_id": "worker-3",
  "clone": "/Users/recursive/Library/Application Support/brehon/agents/<runid>/worker-3",
  "branch": "brehon/runs/<runid>/worker-3",
  "base_ref": "epic/one",
  "head": "<sha>",
  "wip_ref": "refs/brehon/wip/worker-3",
  "wip_tree": "<tree-oid>",
  "ahead": 4, "behind": 12,
  "dirty_count": 7,
  "changed_files": ["crates/a/src/x.rs", "..."],
  "last_write_ts": 1787283935,
  "sandbox_denials_since_last_tick": 0
}
```

`ahead/behind` from `git rev-list --left-right --count <base>...HEAD` — **not** `status --porcelain=v2 --branch`, which emits `# branch.ab` only when an upstream is set, and agent branches are local-only.

### Tick

```bash
# 1. WIP snapshot — non-mutating, in the agent's clone. cp the index, don't read-tree:
cp "$C/.git/index" "$IDX"                                   # 55ms  (read-tree HEAD is 177-331ms)
GIT_INDEX_FILE=$IDX git -C "$C" add -A
TREE=$(GIT_INDEX_FILE=$IDX git -C "$C" write-tree)
[ "$TREE" = "$PREV_TREE" ] || {                             # skip commit-tree on no-change: zero objects
  W=$(git -C "$C" commit-tree "$TREE" -p HEAD -m "wip $agent $ts")
  git -C "$C" update-ref refs/brehon/wip/$agent "$W"; }
rm -f "$IDX"

# 2. Ingest into the operator's repo — this is BOTH the sync and the integration path
git -C "$MAIN" fetch "$C" '+refs/heads/brehon/*:refs/heads/brehon/*' \
                          '+refs/brehon/wip/*:refs/brehon/wip/*'

# 3. Overlap matrix — all local, ~20ms/pair
git -C "$MAIN" merge-tree --write-tree --name-only -z "$wipA" "$wipB"
```

`write-tree` on an unchanged tree writes zero objects; the skip makes the steady state free. Without it you write 1 object per agent per tick forever — 28,800/day at N=20, into a `.git` that is **[V]** already 701 M with 7,529 loose objects and `gc.auto` unset. **Set `gc.auto` explicitly and run `git gc` on a deliberate schedule.**

### Conflict prediction — replace, don't tune

`IntegrationOps::preview_conflicts` (`integration.rs:27`) is a changed-file set intersection, and its doc comments at `:94`/`:105` falsely claim it creates a temporary worktree. Measured on two real pantheon branches: **brehon says 215 conflicting files; `merge-tree` says 0, in 21 ms.** Replace the body.

Three things the lanes got wrong about this change:
- `has_conflicts` (`ops.rs:152`) is **not** dead code — it's the `GitPort` trait impl, mirrored by `mock_git.rs:317`. Blast radius includes 3 test-harness call sites.
- The proposed `<base> <base>` negative control **exits 0** and proves nothing. Use a known-conflicting pair.
- Guard the exit code: a bad ref also exits 1 with empty stdout. `/tmp/workmux-research/src/git/status.rs:403` has exactly this bug and reads a deleted base branch as permanent conflict.
- `git2::Repository::merge_commits` exists and you already depend on it (`merge.rs:95` uses the idiom), but libgit2's merge is not ORT and lacks directory-rename detection. **Take the subprocess** — fidelity to what `git merge` will actually do is the whole point. Write that reason down.

### Scheduling — merge queue, not DependencyGraph

Conflict edges must **not** go into `DependencyGraph::add_dependency`. Conflict is symmetric; `topological_order` (`dependency_graph.rs:133`) errors on the resulting 2-cycles, and `get_ready_tasks` (`:186`) gates task *start* on dependency *completion* — which re-introduces the exact over-serialization the merge-tree change exists to remove, at the wrong phase, on work that has already started.

Instead: a mutual-exclusion set over ready-to-merge branches, resolved greedily at merge time. Merge the largest disjoint batch as one candidate; bisect the batch on CI failure. Bors, not a scheduler.

### Quarantine

Keep the `GIT_INDEX_FILE` snapshot primitive — it's the crash-recovery anchor for uncommitted work and it's free once the WIP tick exists. **Drop the SIGSTOP**: your own defaults are soft-nudge at 5 min, stuck at 10 min (`config.rs:2740-2749`), so a SIGSTOPped agent emits no PTY output and is declared stuck within 10 minutes, and nobody SIGCONTs it on an unattended run. **Drop the tracked-path `git checkout --` revert**: reproduced losing operator-work-v2 from both the file and the quarantine. Move untracked strays into `.brehon/quarantine/`; never `checkout --` a tracked path.

### rerere

`git -C <main> config rerere.enabled true` and **`rerere.autoUpdate FALSE`**. The `.git/rr-cache` sits in the common dir and propagates fleet-wide — which under `autoUpdate true` means one careless agent's "resolve by deleting the incoming hunk" replays **pre-staged and invisible** into every subsequent rebase, and the reviewer panel sees a clean diff with no signal. `false` keeps the entire labor saving (resolution lands in the working tree) while forcing a `git add`, i.e. forcing someone to look. Note the clone layout narrows this to the operator's repo — that's a feature.

### Hygiene

**[V]** `git worktree list --porcelain` emits an explicit `prunable <reason>` field — filter the field, no stat loop. `git worktree prune` clears exactly 76 of pantheon's 81 entries today. Run it plus an `rm -rf` of empty run dirs at session close. That is the whole hygiene fix, and it is worth doing **before** anything else in this document.

### Symbol-level detection

Ship only the cheap half: extract removed-symbol lines from `git diff <base>...<wip_ref>` with a regex over `^-\s*(pub )?(fn|struct|trait|enum|impl|const|static) `, then `git grep` those names across sibling WIP refs. Catches the renamed-API break that `merge-tree` structurally cannot see. Advisory only, never gates a merge. **Defer tree-sitter until the regex produces a false negative someone cares about.**

---

## 6. BUILD ORDER

**Day 1 — free, no architecture change, do these regardless of everything else:**
1. `git worktree prune` + empty-run-dir sweep at session close. Clears 76/81 registrations today.
2. Set `gc.auto` explicitly on pantheon and run a deliberate `git gc`. 7,529 loose objects, 701 M, threshold unset.
3. Replace `preview_conflicts` body with `merge-tree --write-tree --name-only -z`. Net code deletion; 215 false positives → 0. Budget the `GitPort`/`mock_git` contract change.
4. Containment poll in the run loop: `rev-parse HEAD` + rolling status baseline + `config --get core.hooksPath`. ~40 lines. Converts multi-day silent corruption into 60-second latency.
5. Stop printing the protected path in denial messages (`agent_git_guard.rs:195`, `claude_hook.rs:297-302`).
6. `rerere.enabled true` / `rerere.autoUpdate false`.
7. Fix Gemini's autonomy-flag gating — with `worktree_isolation: true` it currently gets no approval flag at all (`gemini.rs:277-283` gates on `allow_privileged_mode`) and will sit at an interactive prompt forever on an unattended run. **This is a live bug costing you agent-days right now, unrelated to containment.**

**Week 1 — the actual fix, in this order (the order is load-bearing):**
8. **L0 clone migration.** `worktree add`→`clone` + `remote remove origin` in `worktree_ops.rs`; one `fetch` in the ledger tick; brehon-owned agent-dir inventory in `.brehon`; per-agent parent directory so `ls ..` shows nothing.
9. **L1 Seatbelt wrapper**, canonicalized paths, the seven profile rules, `.brehon` granted, tmux+docker sockets denied. One real `cargo build` timed inside vs outside before you trust it.
10. **Delete `agent_git_guard.rs`** — *after* 8 and 9 land, never before. Deleting it while the shared `.git` is still reachable removes the only thing standing between a careless `git gc` and a peer's unmerged commits.
11. Ledger + WIP snapshot + overlap matrix + merge queue.
12. `log stream` denial telemetry with a pid-tree mapper.

**Not worth building — explicitly:**
- **Any `chmod -R a-w` or `chflags uchg` freeze of main.** `uchg` is the *user* flag; `chflags -R nouchg .` disarms it in one command as the same uid, and clearing a permission error is the single most predictable careless-model move. It also manufactures the corruption it prevents: with one file frozen, `git checkout other` prints an error, prints "Switched to branch", exits **0**, and leaves that file at the old content. And Cargo.lock is tracked, so cargo in main dies. Root `schg` is the only real version and it costs sudo at both ends plus a repo needing sudo to unstick after a SIGKILL.
- **Inverting the git shim to an allow-list.** Nice idea, moot once L0+L1 land. And it doesn't work as specified anyway: `diff` is on the proposed allow-list and `git diff --output=/abs/path` writes arbitrary files, plus `worktree add`, `clone`, `init`, `archive -o`, `format-patch -o`, `bundle create`, `checkout-index --prefix=` all write to a *destination argument* while `repo_root` is legitimately the agent's own tree.
- **Hiding `BREHON_PROJECT_ROOT`.** Self-defeating: `BREHON_ROOT` is pushed unconditionally and `BREHON_PROJECT_ROOT` is its `.parent()` (`brehon-adapter-sdk/src/lib.rs:364-379`) — brehon's own `main.rs:65-78` does that `dirname` as a documented fallback. Both proposed alternatives write the path into files inside the agent's own worktree. And 6 *tracked* pantheon docs contain it. Keep only the denial-message edit.
- **New PreToolUse hooks as a control.** You already ran that experiment at 768 lines.
- **Launch-refusal for hookless CLIs.** Trades the cross-vendor panel for a partial fix.
- **`extensions.worktreeConfig` + per-worktree `core.hooksPath=/dev/null`.** The agent owns that config file; `git config --worktree --unset` undoes it. Keep only `-c core.hooksPath=/dev/null` on **brehon's own** invocations, threaded through `git_exec.rs::run_git`.
- **The quiescence gate.** `git rev-parse -q --verify REBASE_HEAD MERGE_HEAD CHERRY_PICK_HEAD` returns exit 1 with empty output **unconditionally** (`--verify` takes one revision), so the gate silently passes 100% of the time. And once you publish the new base and let the agent sync itself instead of daemon-rebasing underneath it, there is nothing left to guard. Delete the concept.
- **Per-agent UNIX users.** Breaks the login keychain (OAuth for every vendor), multiplies `~/.cargo`/`~/.rustup` N times, TCC prompts. Correctly rejected — and the clone move gives you real cross-agent isolation for free anyway.
- **Containers / Lima VMs.** The complete boundary, wrong ergonomics: virtiofs stat-storm on a large Rust workspace, punching host holes back through for local Postgres, N toolchains. Threat model is careless, not adversarial. **Do not build this.**
- **`SandboxSpec.read_roots`, `network_class`, `credential_class`** — delete the fields and their merge/validate plumbing. `deny file-read*` breaks rustc, cargo, node and every vendor CLI instantly and opaquely, and it sits in the struct inviting someone to set it. Keep `write_roots`/`denied_roots` **only** as the single `extra_write_roots` knob on the L1 profile generator, generated from canonicalized hardcoded roots.

---

## 7. WHAT SURVIVED REFUTATION AND WHAT DID NOT

### Survived, and is load-bearing
- **Seatbelt is a real kernel boundary on macOS 26.6.1.** Attacked from three directions and held: inherits across fork/exec/`env -i`/login-shell/PTY, **cannot be widened from inside** (`sandbox_apply: Operation not permitted`), `-D` is injection-safe under a policy-closing payload, a missing `-D` is a hard startup error, hardlink-across-boundary is blocked at `link()` not just `write()`, `osascript`/`open(1)`/`launchctl bootstrap` all blocked, execs in place so pid tracking is unaffected.
- **`merge-tree` replaces the overlap heuristic.** 215 → 0, 21 ms.
- **The periodic poll.** 0.017 s on 143,749 files. Free.
- **Fetch-not-push.** **[V]** fetch from a fully hostile agent repo executes nothing; push into one fires 5 hooks + `core.alternateRefsCommand` as the operator.
- **Plain local clone is hardlinked and immune to source `gc`.** **[V]** — this is my correction to both lane 3 *and* its refuter, and it deletes the entire `preciousObjects` branch.

### Broken by the refuters — dropped
- **Granting the shared `.git` to the sandbox** (the "take option (a), it's what Claude Code does" recommendation). **Fatal.** Full destruction chain reproduced. It converts a recoverable stray file into permanent loss of a peer's unmerged commits. This is the single most dangerous item across all four lanes and it was the *recommended* option. Killed by L0.
- **`chmod -R a-w` and `chflags uchg`.** Both same-uid reversible; the `uchg` one additionally produces an exit-0 half-applied `git checkout`.
- **"Stop handing the model the shared-root path."** Obscurity, and self-defeating three ways over.
- **Conflict edges into `DependencyGraph`.** Cycle error plus wrong phase.
- **`stat .git/logs/HEAD` as the "zero-false-positive" tripwire.** Reproduced false negative: `git -C <worktree> update-ref refs/heads/main <sha>` moved main's HEAD and left the mtime **identical** — and `update-ref` is not on the shim's deny-list, so it is the escape that is both permitted and undetected. Use `rev-parse HEAD`.
- **`refs_digest` as specified.** Changed on one ordinary agent commit; ~100% false-positive rate, and it self-triggers on the `refs/brehon/wip/*` refs the design creates. Scope to protected refs only.
- **The quiescence gate's rebase-in-progress check.** Never fires.
- **`clone --shared` + `preciousObjects`.** Unnecessary, and `preciousObjects` makes `git gc` a silent no-op.
- **Launch-refusal gating.**
- **Vendor native sandboxes as an independent second layer.** Gemini's default is `permissive-open`, structurally identical to the proposed profile — same `(allow default)` base, same tmux hole. Same layer twice. Keep only the Codex leg (`workspace-write` covers Codex's edits because it applies them through the shell) and `allowUnsandboxedCommands: false`.

### Already implemented — do not build
- **Closing inherited fds.** portable-pty 0.9 already calls `close_random_fds()` in `pre_exec` (`unix.rs:276`) and every spawn routes through `pty/core.rs:121`. Make it an assertion, not a feature.
- **A PreToolUse file-path hook.** 768 lines, shipped, and it is the evidence for the thesis, not a proposal.
- **`ensure_worktree_sandbox_enforced`** (`run/mod.rs:3132`) already refuses `worktree_isolation: true` + `sandbox_profile: None`.

### Survived but with the cost restated honestly
- **`log stream` telemetry.** Works, no coalescing under a 300-denial burst — but it needs a live pid-tree mapper with PID-reuse handling, not "no extra code", and it does nothing for the diffing ask.
- **`sandbox-exec` deprecation.** Man-page-deprecated, no runtime warning, Chrome and Codex depend on it. Acceptable; revisit if a major macOS release removes it.
- **The `cargo build` overhead number.** "Zero" rests on a synthetic stat/open microbenchmark in a hot policy cache. Unverified for the real workload. Time it before you trust it.

### The one thing nothing here closes
**Reads.** `Read`/`Grep`/`Glob`/`LS` and `ls ..` stay open by design, so the model can always *learn* about the operator's repo. L0 removes most git-sourced paths and L1 makes the knowledge inert. Denying reads breaks rustc, cargo, node and every vendor CLI opaquely — the cure is worse. Accept it and stop trying to fix it.

<!-- Generated 2026-08-21 by a 9-agent adversarial workflow (4 lanes + 4 refuters
     + synthesis, Opus 5, ~967k tokens). [v] = reproduced on this machine.
     SUPERSEDES the clone recommendation in BREHON-CONTAINMENT-2026-08.md:
     path clones hardlink packfiles -- use file:// . -->

# Transport & confinement: ranked options

Everything below marked **[v]** was reproduced on this machine this session (git 2.50.1 Apple Git-155, macOS 26.6.1); **[lane]** = reproduced by a lane and survived its refuter; **[doc]** = git/kernel documentation.

---

## 1. THE FRAMING

Three orthogonal layers. Conflating them is what made every lane rank badly.

| Layer | Question it answers | What it cannot do |
|---|---|---|
| **Topology** | Which repos exist, who owns each, what names appear in whose config | Stop anything. It only decides *blast radius when the other two fail*. |
| **Transport** | How bytes move between them | Authenticate. Every scheme on your list runs at one uid with reads open, so any credential it uses is world-readable to peers. |
| **Confinement** | What the kernel refuses | Protect against an alias. It authorizes by *path*; hardlinks are a second path to one inode. |

Two facts collapse most of the option space:

**(a) Transport buys you nothing here.** O5 token, O6 key, O4 broker-id: all are strings in files or argv that every peer agent can read. Verified consequence, not theory — the broker lane's own O8 recommendation was broken by editing `remote.origin.url` in the agent's own `.git/config` and deleting a ref inside another agent's namespace. Identity requires a boundary the fleet does not have (distinct uids, or a socket path the kernel pins). Everything else is a label.

**(b) Confinement is where cross-platform pressure lands, and it does not port cleanly.** The topology is `clone` + `fetch` — byte-identical on macOS, Linux, Windows. The confinement layer is three different products with three different holes: Seatbelt (real, deprecated, macOS), Landlock (real, but no deny rules, no chmod/chown/utime ever, no truncate below kernel 6.2, no signal scope below 6.12), bubblewrap (needs unprivileged userns — AppArmor-restricted on Ubuntu 23.10+), Windows (nothing). So: **pick the topology whose safety argument survives the confinement layer being weak, absent, or wrong**, because on some platform it will be.

And the fact that overrides all three layers:

> **`git clone <path>` hardlinks packfiles. The agent's own `.git/objects/pack/*.pack` *is* canonical's inode.** [v] this session: path clone → `nlink=2`, same inode; `chmod u+w` + truncate **inside the agent's own directory** → source repo reports `far too short to be a packfile`. `git clone file://<path>` → `nlink=1`, disjoint. `git fetch` never hardlinks at all [v].

No sandbox on any platform closes that, because the write target is inside the directory the sandbox must leave writable, and Landlock never restricts `chmod(2)` [doc]. The trigger is the stated threat model verbatim: naive write fails with `Permission denied` (packs are 444), careless model runs `chmod -R u+w .`, canonical is silently gone. **The single highest-value edit in this entire investigation is one word in the clone command.**

---

## 2. THE RANKED TABLE

Naming convention: `′` = the corrected form (`file://` clone, no path-hardlinks), which is not what any lane originally described.

| # | Option | Safety unconfined | Safety confined | Cross-platform | Moving parts | Agent can sync from base? | brehon cost | Safety rank | Design rank |
|---|---|---|---|---|---|---|---|---|---|
| **O2′** | **Base mirror: `base.git` bare, brehon-writable, agent-READ-only; agents `clone file://base.git`; agent pushes nowhere** | Good. Agent's `origin` is a disposable 22M mirror; canonical not on any command the agent already types. Object stores fully disjoint. | Best. One writable root per agent, `denied_roots` empty (full Landlock parity). Worst case = re-clone. | Identical everywhere. Pure filesystem. | 1 bare repo + 1 refresh command + staleness signal | **Yes** — stock `git fetch origin` | ~40 LOC + refresh job | **2** | **1** |
| **O0′** | Amnesiac clone: `clone file://`, `remote remove origin`, `rm -rf .git/logs` | Best. Nothing named anywhere in `.git` [lane]. | Same as O2′ plus naming. | Identical everywhere. | Zero. | **No** — the disqualifier on multi-day runs | Lowest | **1** | 3 |
| **O1′** | `origin` = canonical via `file://`, read-only by confinement | Weakest credible. `git push origin`, `git gc`, `git remote prune origin` all target canonical, and those are commands the model types unprompted. Exposes *all* canonical refs incl. peers' branches. | Good [lane]: upload-pack needs no write access; push fails `unable to create temporary object directory`. All safety borrowed from the sandbox. | Topology identical; **safety** is not — this is the option where the Landlock/Windows gap actually bites. | Zero. Cannot go stale. | Yes, instantly | ~0 | 4 | **2** |
| **O3** | git bundle exchange | Very good. Inert file, no live connection, no config naming anything. | Excellent, and **uniquely does not depend on the sandbox at all**. | Most portable; the only honest answer where confinement can't be established. | Bundle lifecycle: per-agent base-SHA tracking, rotation, GC, prereq failures. Real state brehon must own. | Yes, but non-native (`git fetch <file>`); models handle prereq errors badly | 150–250 LOC | **3** | 5 |
| **O4** | `ext::` broker (± `GIT_NAMESPACE`) | Negative. Requires flipping `protocol.ext.allow` off its deliberate `never` default → re-enables `.gitmodules`-driven RCE fleet-wide [lane]. Broker id is client-asserted [lane: agent1 deleted agent2's ref]. | Only as strong as per-agent socket pinning — **macOS-only in practice**: Landlock cannot restrict unix-socket connect until ABI v9 = Linux 7.1 [doc]; no shipping LTS is there. | Worst. macOS realpath traps, Linux needs mount namespaces (privileged), Windows needs a different IPC entirely. | Most: listener supervision, N sockets, shipped bridge binary (no socat on macOS), policy engine, per-platform profiles. Denial-by-disconnect makes git print `* [new branch]` for a push that did not land [lane]. | Yes | Highest | 6 | 7 |
| **O5** | Local HTTP + per-agent token | Poor. Token lives where git can read it = where peers can read it. | No better than O2′, plus a listening port every local process can reach. All agents are `127.0.0.1`, so no sandbox can separate them by peer address [lane]. | Cleanest *transport* cross-platform, and the only one that extends to cross-machine. `git-http-backend` is not on PATH (libexec only) [lane]. | Server, port allocation, token mint/rotate/revoke, CGI env. | Yes | High | 7 | 6 |
| **O6** | ssh + git-shell + forced command | Poor (keys readable by peers) and requires enabling Remote Login — a machine-wide network service to solve a local problem. sshd is currently off [lane]. | No gain; forces a writable `~/.ssh` hole next to the operator's real keys. | Worst operational variance of the set. | sshd, host keys, N keypairs, authorized_keys, known_hosts churn. | Yes | High + host admin | 8 | 8 |
| **O7** | `git daemon` | **Disqualified.** [lane] verified an anonymous client *deleting a branch* in canonical. Man page: "anybody can push anything ... including removal of refs". | Unfixable: `--access-hook` sees only `127.0.0.1` for every agent. | Ships everywhere; that's all. | Daemon, port, export markers. | Yes | Low (the trap) | 9 | 9 |
| **O8** | **Per-agent uid** (orthogonal to topology) | Strongest primitive available. Restores `safe.directory` (inert today, 501==501 [lane]), makes file modes mean something, stops peers reading each other's config — **and is the only thing that closes the hardlink channel without `file://`**, because `chmod u+w` on a 444 root/other-owned pack requires ownership. | Composes with everything; would let you replace most SBPL with `0700`. | The only option with a credible **Windows** story (accounts + ACLs). Works identically macOS/Linux. | Root to provision; each agent uid needs its own `$HOME` with valid vendor CLI auth (`~/.claude`, `~/.codex`, …). That's a licensing problem as much as a technical one. | Yes | Highest, mostly outside brehon | **0 (best)** | 4 (deferred) |
| O9 | Container per agent | n/a | Very strong; agent cannot name the host path. | Most uniform, and the honest Windows answer. | Runtime, images, volumes, PTY-through-container. | Yes | High | 3= | 6= |
| O10 | Partial clone `--filter=blob:none` | Worse than plain: permanent runtime coupling to a promisor, forfeits gc-immunity. | Worse. | Portable, with its failure modes. | New class of intermittent lazy-fetch failure. | Yes | Low to add, high to debug | reject | reject |

### Where safety and design disagree — three real splits

1. **O0′ is safer than O2′; O2′ is better designed.** O0′ names nothing at all. But on the multi-day unattended runs brehon exists for, "cannot sync from base" converts every clean rebase into a large conflict at exactly the moment the reviewer panel is gating. O0′ always terminates in O2′ or O3 the moment you need sync — ship O0′ only for short single-shot tasks.
2. **O3 is the only option whose safety does not depend on the confinement layer.** On any host where you cannot establish Seatbelt/Landlock (Windows-native, Debian 12 / RHEL 9 where Landlock ABI < 3 cannot even deny `truncate(2)` [doc]), O3 outranks everything. Design says its bookkeeping isn't worth it on macOS/modern-Linux. Keep it as the recovery/air-gap path, not the steady state. (Correction to a lane claim: bundles are ~21M on pantheon, not 701M, and `git clone <bundle>` from an `--all` bundle checks out fine — the "no HEAD" finding was an artifact of how the bundle was cut. O3's cost is *state*, not disk.)
3. **O8 is the only complete answer and the only Windows path, and you should still defer it.** Trigger conditions in §3.

---

## 3. THE RECOMMENDATION

**Primary: O2′.**

```
canonical (brehon-write, agent-DENY-write)
   │  git clone --bare file://canonical  base.git          # 1.16s, 22M, nlink=1  [v on pantheon]
   │  refresh: git -C base.git fetch --prune file://canonical '+refs/heads/main:refs/heads/main'
   ▼
base.git (brehon-write, agent-READ-only, TRUSTED — brehon may run git in it)
   │  git clone file://base.git  <agentdir>                # 0.47s, 22M, nlink=1  [v]
   ▼
agent repo (agent-write, brehon NEVER cwd's here, agent pushes NOWHERE)
   │  brehon: git -C canonical fetch --no-tags <agentdir> \
   │            'refs/heads/brehon/*:refs/heads/brehon/<agent-id>/*'      # no leading '+'
   ▼
canonical  ── cherry-pick -x, reviewer panel, is_safe_brehon_branch unchanged
```

Total disk for 8 agents on pantheon: ~200M against a 701M canonical. Per-agent seed is sub-second.

This is **not** the O2 any lane described. The difference is load-bearing: in every lane's O2 the agent *pushed into* its mirror, which made the mirror agent-writable, which made brehon's mandatory refresh `git -C <agent-writable> fetch …` — the forbidden direction, verified to fire planted `reference-transaction` hooks as brehon, on a schedule, forever. Here the agent writes **only its own clone**; `base.git` is brehon-owned state, so running git inside it is as safe as running git in canonical.

**Deferred upgrade: O8 (per-agent uid).** Build it when any one of these becomes true:
- an agent is no longer same-trust with the operator (third-party model, untrusted prompt source);
- you need vendor credentials/caches isolated per agent (today they cannot be — see §7);
- you go multi-machine, at which point O5-over-the-wire stops being theatre because a server enforces identity.

**Do not build (elegant, not worth the code):**
- **The `ext::` broker + `GIT_NAMESPACE`.** The most intellectually satisfying option here and the loudest "no". It requires disabling a git safety default fleet-wide, its identity is a string the agent owns, its enforcement mechanism is macOS-only until Linux 7.1, its failure mode is every agent's git breaking when brehon restarts — which teaches the model to "fix" `origin` by pasting the canonical path — and its headline ergonomic advantage doesn't exist (a fresh namespace is empty; brehon must seed base refs into every namespace, the exact chore it charges against O2′). It also writes unreviewed agent objects directly into canonical (one agent pushed a 60M blob → canonical 112K → 60M, survives `gc --prune=now` because the namespace ref makes it reachable [lane]).
- HTTP server, sshd, git-daemon, gitoxide/libgit2 as a wire-protocol server (gitoxide's own `crate-status.md` leaves push, send-pack/receive-pack, and server plumbing unchecked; libgit2 #5605 open since 2020).
- A new sandbox policy type. `SandboxSpec` already exists — §7.
- `extensions.preciousObjects` (already established as unnecessary), `chmod -R a-w base.git` (owner can chmod back at one uid [lane]; also breaks your own refresh).

---

## 4. THE CODE-EXECUTION MATRIX

Definitive for the surviving transports. "As brehon" = unconfined, operator privileges.

### Direction A — brehon in canonical, agent repo as remote **source** (the integration path)
`git -C canonical fetch|ls-remote|clone <agentdir>` (bare path **or** `file://`)

Note the mechanism, because it is the crux: **git does spawn `git upload-pack <agentdir>` as brehon with the agent's repo as GIT_DIR.** It is inert because git deliberately hardens exactly this path.

| Agent-controlled knob | Fires? | Why |
|---|---|---|
| `uploadpack.packObjectsHook` | **No** | Protected-configuration scope only (system/global/`-c`). Man page: *"a safety measure against fetching from untrusted repositories."* [doc][lane] |
| `core.alternateRefsCommand` | **No** | receive-pack advertisement path only. Re-tested with a real `objects/info/alternates` entry present — still inert [lane]. |
| any hook | **No** | No server-side fetch hook exists in git 2.50. Full `githooks(5)` list contains no `post-upload-pack` [lane]. |
| `extensions.partialClone` promisor → `ext::` canary | **No** | upload-pack does not lazy-fetch; rc=128, canary never ran [lane]. |
| `core.fsmonitor`, `filter.*`, `diff.*`, `core.pager` | **No** | Client-side; they apply to canonical (trusted), not the source. |
| refspec escapes / tags | **No** | `refs/heads/brehon/../../evil` → `refusing to update ref with bad name`; tags do not follow when an explicit refspec is given [lane]. Still pass `--no-tags --no-recurse-submodules`. |

**Result: nothing executes. This is the only safe way to touch an agent repo.** Four independent attack attempts across lanes failed to break it.

### Direction B — brehon pushes **into** an agent repo
`git -C canonical push <agentdir> …` → `git receive-pack <agentdir>` as brehon.

Fires, as brehon: `pre-receive`, `update`, `post-receive`, `post-update`, `reference-transaction`, `core.alternateRefsCommand` (+ `proc-receive` / `push-to-checkout` if configured). **Never do this.** Six code paths, verified [lane], reconfirmed by three refuters.

### Direction C — brehon runs git **with the agent dir as cwd/`-C`/GIT_DIR**
Everything in the agent's repo-local config is brehon's. `safe.directory` gives zero protection (same uid) [lane].

| Knob | Triggers | Killable? |
|---|---|---|
| `core.fsmonitor` | any index refresh: `status`, `diff`, `add`, `ls-files`, `commit`, `checkout`, `merge`, `blame`, `gc` | **Yes** — `-c core.fsmonitor=false` (or `=`; both accepted [v]) |
| hooks: `post-index-change`, `post-checkout`, `reference-transaction`, `pre-commit`, `prepare-commit-msg`, `commit-msg`, `post-commit`, `post-merge`, `post-rewrite`, `pre-rebase`, `pre-auto-gc`, `pre-push` | matching operation | **Yes** — `-c core.hooksPath=<brehon-owned empty dir>`. **Not** `/dev/null` (that's `NUL` on Windows), and **never globally** — it disables `BREHON_PROTECTED_BRANCH_HOOKS` (setup.rs:24, installed at :1844) including the `reference-transaction` guard on the very fetch that writes agent refs into canonical. |
| `diff.external`, `diff.<drv>.command` | `diff`, `log -p`, `show`, `diff-tree -p`, `blame` | **Yes** — `--no-ext-diff` (accepted by `diff-tree` [v]) and `-c diff.external=` |
| `diff.<drv>.textconv` | same | **Yes** — `--no-textconv` (accepted by `diff-tree` [v]) |
| `core.pager` | pager-using commands **only when stdout is a TTY** | **Yes** — `--no-pager`. Correction to a lane claim: brehon-mcp is **not** exposed — `run_command_hardened_with_env` sets `.stdout(Stdio::piped())` (git_exec.rs:219). brehon-cli's `.status()` sites do inherit the terminal but use `project_path` (canonical) as cwd. |
| `core.sshCommand`, `core.askPass`, `credential.helper`, `gpg.program`, `core.editor`, `sequence.editor`, `core.gitProxy` | protocol / signing / editor paths | **Yes** — `-c <key>=` each |
| **`filter.<attacker-name>.clean/smudge/process`** | `add`, `checkout`, `status`, `stash`, `diff`, **`merge`**, **`cherry-pick`**, `rebase`, `archive` | **NO. Unkillable by any `-c`.** The driver name comes from in-repo `.gitattributes`, so it cannot be enumerated. Verified firing under the *complete* hardening vector on `merge` and `cherry-pick` [lane]. `.process` is a long-running coprocess. |
| **`merge.<attacker-name>.driver`** | `merge`, `cherry-pick`, `rebase`, `stash pop` with a conflicted path | **NO.** Same `.gitattributes`-selected-name problem. |
| `alias.<builtin-name>` | — | **n/a** — git ignores aliases that shadow builtins ([v]: `alias.fetch=!echo ALIAS_FIRED` set, real fetch ran). |

**Consequence — the rule the codebase can actually honour:** "brehon never touches an agent repo" is unachievable (see §9). The achievable rule is:

> **Inspect agent repos via libgit2. Mutate only in canonical. Everything else gets the Tier-B flags.**

libgit2 runs no hooks, no fsmonitor, no filters, no textconv — immune by construction. brehon already depends on it and already uses it here (`Repository::open` at brehon-git/src/worktree.rs:179, :469, :598; `*_with_git2` in factory/worktree_ops.rs). That's why `worktree_state_check` / `is_worktree_clean` / `archive_worktree` survive the topology change untouched — a fact worth generalising into policy rather than rediscovering.

### Direction D — agent fetches from `base.git`
`upload-pack` runs in `base.git` **as the agent**. `base.git` is brehon-owned, so nothing agent-controlled executes and nothing brehon-controlled is at risk. Agent push into `base.git` fails at the sandbox (`unable to create temporary object directory` [lane]).

### Direction E — the PATH channel (not a git direction, but the same class)
brehon spawns `Command::new("git")` (git_exec.rs:66/176/186 and 110 raw sites in brehon-cli). That resolves through **brehon's** PATH, which includes `~/.cargo/bin`. If `~/.cargo` is in `write_roots`, an agent drops a `git` there and brehon executes it. **Resolve git to an absolute path once at startup.** See §7.

---

## 5. HARDENING FLAGS

Two tiers. Threading one global vector through `run_git` **breaks brehon** — verified: `-c protocol.allow=never` makes `ls-remote https://github.com/…` fail `transport 'https' not allowed`, and canonical has a real GitHub origin.

**Tier A — always, in `run_git` (git_exec.rs:65):**
```
--no-pager
-c core.fsmonitor=false
-c core.alternateRefsCommand=
-c gc.auto=0
```
Plus what's already there (`GIT_TERMINAL_PROMPT=0`, `GIT_ASKPASS=echo`, `SSH_ASKPASS=echo`, `GIT_OPTIONAL_LOCKS=0`, apply_git_non_interactive_env at :425), plus **git resolved to an absolute path**.

**Tier B — only on calls that touch an untrusted repo (agent dir as cwd, and the fetch-back):** Tier A, plus
```
-c core.hooksPath=<brehon runtime empty dir>      # NOT /dev/null; NOT global
-c diff.external=  --no-ext-diff --no-textconv    # diff-family only
-c core.sshCommand=  -c core.askPass=  -c credential.helper=
-c gpg.program=false  -c core.editor=false  -c sequence.editor=false
-c fetch.fsckObjects=true  -c transfer.fsckObjects=true
env: GIT_CONFIG_NOSYSTEM=1  GIT_CONFIG_GLOBAL=<null>   # brehon already handles the Windows NUL case in setup/agent_git_guard.rs
```

**On the fetch-back specifically:**
```
git -C <canonical> fetch --no-tags --no-recurse-submodules <agentdir> \
    'refs/heads/brehon/*:refs/heads/brehon/<agent-id>/*'
```
plus a **per-call timeout override** — `DEFAULT_GIT_TIMEOUT = 60s` (git_exec.rs:55) now gates a real data-transferring operation that did not exist under worktrees.

**Seed clone:** `git clone file://…` — add `-c protocol.file.allow=always` only if you ever pin `protocol.allow`.

**Explicitly do NOT set:**
- `-c protocol.allow=never` anywhere near a real remote (breaks https/ssh) — and its stated rationale was wrong anyway: `url.<base>.insteadOf` is client-side config read from the repo brehon runs *in*, never from the remote.
- `-c core.hooksPath=…` globally (kills your own protected-branch guards).
- Any belief that Tier B is a boundary. It is defense-in-depth. `filter.<name>.*` and `merge.<name>.driver` walk through all of it.

---

## 6. BASE-UPDATE CHANNEL (the hole in the prior recommendation — closed)

Constraints, all verified:
1. brehon must not run git inside an **agent-writable** repo (Direction C);
2. brehon must not push into an agent repo (Direction B);
3. the agent must reach base with stock git, or vendor CLIs and models won't do it;
4. no hardlink alias may exist between agent-writable objects and canonical.

**Answer: a base mirror that is brehon-writable and agent-read-only, that the agent never pushes to.** That single ownership split satisfies all four — it is why the direction rule is not violated by refreshing it.

```bash
# seed (once per project)
git clone --bare file://$CANONICAL $BASE          # 1.16s / 22M / nlink=1 on pantheon [v]
git -C $BASE remote remove origin                 # canonical's path appears nowhere agent-readable
git -C $BASE config gc.auto 0

# refresh (brehon-owned, safe: $BASE is trusted state)
git -C $BASE fetch --prune file://$CANONICAL '+refs/heads/main:refs/heads/main'
date -u +%FT%TZ > $BASE/BREHON_REFRESHED_AT

# seed an agent
git clone file://$BASE $AGENT                     # 0.47s / 22M / nlink=1 [v]
rm -rf $AGENT/.git/logs                           # reflog otherwise records "clone: from $BASE"
git -C $AGENT config gc.auto 0
```

Design notes that matter:

- **Narrow the refspec to the base branch(es).** `base.git` then exposes *only* base — not every other agent's integrated `brehon/*` branches, which O1′ (origin = canonical) hands to everyone.
- **Refresh cadence is a brehon decision, and that's the feature.** You can pin the whole fleet on a known-good base while you land something risky. O1′ can't.
- **`fetch` never hardlinks** [v: mirror packs `nlink=1` after a local-path fetch], so refresh can't reintroduce aliasing even if someone later seeds with a bare path.
- **Staleness must be observable, not assumed.** If the refresh job dies at hour 6 of a 72-hour run, every agent silently develops against a frozen base and conflict rates climb with no signal. Surface `BREHON_REFRESHED_AT` age in status/doctor. One line each side. (This is the one place O1′ is genuinely better: it structurally cannot go stale.)
- **Tell agents to `git merge origin/main`, not `git rebase`.** Rebase rewrites SHAs. Sequence: agent commits X → panel approves X → base advances → agent rebases → tip is X′ → brehon integrates a commit no panel ever reviewed, with `cherry-pick -x` recording provenance that looks legitimate. Merge preserves reviewed SHAs, and cherry-pick integration already ignores merge commits.
- **Belt and braces on the same hazard:** record the reviewed tip SHA at approval and refuse integration if the tip moved (~10 LOC). Dropping the `+` from the fetch-back refspec is a free tripwire for the same thing — a rewritten history arrives as a loud non-fast-forward failure instead of a silent force-update.
- **Per-agent destination namespace is mandatory**, independent of base sync. Two agents that both name a branch `brehon/w1` (a model copying a name out of context) silently force-overwrote each other in canonical, and the loser's reviewed commit was unreachable after gc [lane, reproduced twice independently]. Worktrees prevented this (`fatal: a branch named 'brehon/w1' already exists`); clones do not. `is_safe_brehon_branch` already accepts nested names (`brehon/runs/session-a/worker-2`, reset.rs:429), so the guard is unaffected.
- **Bound the fetch-back.** Nothing gates what `git -C canonical fetch <agentdir>` pulls in; one agent that commits `target/` permanently inflates canonical. Under worktrees this was a non-event (shared object store, already counted). Add a post-fetch object-count/size guard.

---

## 7. CONFINEMENT ABSTRACTION

**Do not design a policy type. It already exists and is dead.** `brehon_types::config::SandboxSpec` (brehon-types/src/config.rs:2310) is exactly the requested shape — `{ backend: SandboxBackend::{None,OsDefault,Bubblewrap,Seatbelt}, read_roots, write_roots, denied_roots: Vec<FsRootSpec{path,recursive}>, network_class, credential_class, env_policy, unsafe_marker }` (`SandboxBackend` at :2199, `FsRootSpec` at :2300). It is serde'd, merged, validated, unit-tested — and the only non-test consumer, `agent_env.rs:56`, takes `.profile` (the *name*) and **discards `.spec`**, exporting `BREHON_PERMISSION_PROFILE` for the vendor CLI to self-apply. Today's confinement is an honour system. Write the compiler; design nothing.

**No trait.** A per-platform trait with one impl each, never dispatched polymorphically, is an interface with one implementation. `SandboxBackend` is already an enum — match on it.

```rust
// crates/brehon-mux/src/pane/confine.rs — beside grok_sandbox.rs
pub struct ConfineCtx<'a> { pub work: &'a Path, pub runtime_dir: &'a Path }

/// Ok(vec![]) ONLY when spec.unsafe_marker. Err = fail closed, refuse to spawn.
pub fn confine_argv(spec: &SandboxSpec, ctx: &ConfineCtx) -> Result<Vec<String>>;

pub fn probe() -> ConfineSupport;   // wire into crates/brehon-doctor/src/checkers
pub struct ConfineSupport {
    pub backend: Option<SandboxBackend>,
    pub detail: String,
    pub unenforceable: Vec<&'static str>,   // must be shown BEFORE a run, not after an incident
}
```

**Argv prefix, not `pre_exec`.** `Pty::spawn` (brehon-pty/src/pty/core.rs:104) goes through `portable_pty::CommandBuilder`, which exposes no `pre_exec` and whose `as_command()` is `pub(crate)` — portable-pty consumes `pre_exec` itself for setsid/TTY. So Landlock-via-`pre_exec` is unavailable at the primary spawn path. Compile to a prefix instead; it's the shape `sandbox-exec` and `bwrap` already have and it works unchanged at the second choke point (adapter-sdk/src/process.rs:99, which *does* have `pre_exec` at :123 — keep that as an escape hatch, but don't fork the design over it). `sandbox-exec` execs in place, so setpgid / kill_on_drop / reaper logic is untouched [lane].

**Backends:**

| Platform | Prefix | Real properties | Unenforceable — put these in `ConfineSupport.unenforceable` |
|---|---|---|---|
| macOS | `sandbox-exec -f <profile.sb> -D OWN=<realpath>` | Blocks writes **and metadata**: `chmod`, `utime`, `ln`, `truncate`, and symlink traversal into a denied root all EPERM [lane]. Inherits across fork/exec/PTY, cannot be widened from inside. | setuid binaries cannot exec under *any* profile — `ps`, `top`, `sudo`, `su`, `crontab`, `traceroute` all fail (`pgrep`/`pkill`/`lsof`/`netstat` are fine). Document the substitutes; agents shell out to `ps` constantly. |
| Linux | `<current_exe> confine --policy <file> --` (landlock crate 0.4.7, MSRV 1.71, no root, no daemon) | Unprivileged via `PR_SET_NO_NEW_PRIVS`, inherited across execve, irreversible from inside — a genuine peer of Seatbelt on the properties that matter. | **`chmod`/`chown`/`utime`/`setxattr` — never, on any ABI** [doc]. **`truncate` below ABI v3 = kernel 6.2** [doc: *"truncating files does not require LANDLOCK_ACCESS_FS_WRITE_FILE"*]. **Signal scoping below ABI v6 = 6.12.** No REFER (cross-dir rename) below v2 = 5.19. Pre-opened fds, `/proc`, `/dev/fd`, pipes, sockets: never restricted. |
| Linux (escalation) | `bwrap --ro-bind …` | Only way to get EROFS on metadata, and the only way to express a `denied_root` nested inside a `write_root`. | Needs unprivileged userns — AppArmor-restricted by default on Ubuntu 23.10+, off on RHEL7-era. Not a default. |
| Windows | — | **Err.** No primitive for "run this arbitrary CLI, deny writes outside this dir". AppContainer needs per-object SID ACLs on binaries you don't own; Job Objects govern CPU/memory, not paths; restricted tokens mean mutating ACLs on the operator's real repos. | Everything. Error message names WSL2 (where the Linux path applies unchanged) and containers. |

**Policy-shape invariants the compiler must enforce (not discover):**
1. **`denied_roots` must be disjoint from `write_roots`.** Landlock has no deny rules, only allow-grants over a default-deny handled set; a nested deny is *unexpressible* there and enumerating siblings breaks `git checkout` creating new top-level entries. **Error at compile time** rather than silently under-enforcing on one platform.
2. **Emit order: read_roots → write_roots → denied_roots.** SBPL is last-match-wins; deny-before-allow is silently defeated [lane]. `SandboxSpec` declares the fields in that order, so naive iteration happens to be correct — that is luck. Golden-test the emitted profile text.
3. **Canonicalize every path** (`realpath`) before templating. `/tmp` vs `/private/tmp` makes the whole profile inert; it fails closed but looks like a broken bridge [lane, three separate lanes hit it].
4. **Reject a `write_root` that is an ancestor of any read-only root, of canonical, of `base.git`, of the brehon runtime dir, or of the emitted policy file.** A lane destroyed its own test canonical this way with a blanket `/private/tmp` grant. Grant per-user `$TMPDIR`, never `/tmp` — and better, set `TMPDIR` per-agent under the work root (`git init/add/commit` needs no tmp grant at all [lane]; note macOS `mktemp -t` uses `_CS_DARWIN_USER_TEMP_DIR` and ignores the override, so nothing brehon owns may live in the shared dir).
5. **Assert no file under `<agentdir>/.git/objects` has `st_nlink > 1` at spawn.** One stat walk; catches a bare-path clone regression before it can eat canonical.

**The cache-grant hazard — grant leaves, never parents.** Under default-deny writes, `~/.cargo`, `~/.rustup`, `~/.npm`, `~/.cache`, `~/.claude`, `~/.codex` are all denied, and cold builds plus every vendor CLI's session state break. But granting the parents is a **full escape to the operator's unconfined shell**, verified: `~/.cargo/bin` is on PATH and a confined agent wrote an executable there that the operator's own shell resolved; `~/.rustup/toolchains/*/bin/` holds `cargo`/`clippy-driver`; `~/.claude/settings.json` has a live `hooks` key and Claude Code executes it. A careless `cargo install` trips this with no intent. So:
- grant `~/.cargo/registry`, `~/.cargo/git`, `~/.rustup/downloads`, `~/.rustup/tmp`, `~/.npm/_cacache` — **never the parents**;
- redirect `CLAUDE_CONFIG_DIR` / `CODEX_HOME` per-agent into the work root (one env var, removes the class);
- note this makes the leaf form **mandatory on Linux** — "grant `~/.cargo`, deny `~/.cargo/bin`" is exactly the nested-deny case Landlock cannot represent, so macOS and Linux would silently disagree on the rule that matters most.

**Signals — the hole nobody looked at.** `(allow default)` leaves signals open; a confined agent killed an out-of-sandbox process [lane]. `pkill -f node` / `killall cargo` is normal "clean up my dev server" behaviour and it ends a 72-hour run. Two lines, verified:
```scheme
(deny signal)
(allow signal (target self) (target children))
```
**Not** `(target pgrp)` — that re-opens the hole whenever the victim shares a process group. On Linux this needs `LANDLOCK_SCOPE_SIGNAL`, ABI v6 / kernel 6.12, so on most kernels in service it does not exist: `unenforceable`.

**Fail-closed rule:** `confine_argv` returns `Ok(vec![])` **only** when `spec.unsafe_marker` is set — the field already exists, add no new config key. Any other inability to build a real boundary is `Err`, and `Err` means *refuse to spawn the agent*, loudly, naming the reason. Linux floor: **Landlock ABI ≥ 3**, `Err` below it (not `CompatLevel::BestEffort`, which silently degrades past the truncate hole); build the handled set with `AccessFs::from_all(abi)` so TRUNCATE can never be hand-dropped.

**One call site, and it is not where the precedent sits.** `apply_grok_acp_hardening` is called from two places (spawn.rs:1142, :1919); a third spawn path would silently miss confinement. Apply inside `Pty::spawn` with a **required** (non-`Option`) `confine: SandboxSpec` field on `PtyConfig` so the compiler forces every present and future site to choose — brehon-pty already depends on brehon-types. Caveat: that guarantee dies the moment someone writes `..Default::default()`, and `impl Default for PtyConfig` exists at pty/config.rs:68 (used only by tests and a doc comment). Delete it if you want the invariant to be real. The user's own terminal (pane/terminal.rs:76) then passes an explicit `unsafe_marker: true` — visible and deliberate.

**Landlock implementation trap:** `restrict_self()` is per-thread by default, and brehon's entry point is `#[tokio::main]` (brehon-cli/src/main.rs:376). Re-entering `<current_exe> confine` means worker threads already exist and you confine one thread of a process that then execs the agent — fails silently, writes succeed, and a golden test on argv text still passes. **Intercept `confine` in a non-async `main()` on raw `std::env::args()` before the runtime is built**, and call `all_threads()` as belt-and-braces.

**Testable in two cheap pieces:** `confine_argv` is pure → golden-test the emitted SBPL/argv (deny-after-allow ordering; nested-deny error; realpath). Plus **one** integration test that spawns through the real prefix and asserts an out-of-root write fails. No framework, no fixtures.

---

## 8. WHAT SURVIVED REFUTATION AND WHAT DID NOT

**Survived (build on these):**
- **`fetch`/`ls-remote`/`clone` FROM a hostile agent repo executes nothing** — the one load-bearing result, attacked four ways it hadn't been (`uploadpack.packObjectsHook` from local config; `core.alternateRefsCommand` *with* a real alternates entry; the absence of any upload-pack hook in 2.50; a partial-clone promisor pointing `remote.origin.url` at an `ext::` canary). All inert, with liveness proven in the same repos. It rests on documented, intentional git behaviour, not a version accident.
- **Refname/refspec containment.** Namespace escapes and tag smuggling both blocked by git's own validation.
- **Seatbelt is a real kernel boundary**, including metadata (`chmod`/`utime`/`ln`), `truncate`, and symlink traversal into a denied root.
- **`SandboxSpec` exists and is dead** — the single laziest and most valuable instruction in the whole investigation.
- **O5/O6/O7 rejections**, on shared-uid and (for O7) verified anonymous ref deletion.
- **SBPL last-match-wins; realpath required; setuid unexecutable under Seatbelt; landlock 0.4.7 maturity; the Landlock unrestrictable list.**

**Broken — including things lanes recommended:**

| Claim | Verdict |
|---|---|
| "Hardlinked local clones are free and immune to source gc" (the *already-blessed* prior recommendation) | **Fatal.** gc-immunity is about `unlink`; it says nothing about in-place writes. Reproduced this session: agent truncates its **own** pack after `chmod u+w`, canonical dies. No confinement layer on any platform closes it. |
| "O2 wins: the mirror is disposable, canonical never named" (lane 1 + lane 2 + lane 3 top pick) | **Fatal, twice.** `clone --bare` writes `remote.origin.url = <canonical>` into a 0644 file the agent's own origin points at; and because the agent *pushed into* the mirror, the mandatory refresh ran brehon's git inside agent-writable state and fired planted `reference-transaction` hooks four times as brehon. Fixed here by the ownership split (agent reads the mirror, pushes nowhere). |
| "O2 removes canonical structurally; path-invisibility is the ranking criterion" | **Broken.** Reads are open by design; `ls ~/workspace` enumerates canonical from inside the sandbox. Demote to *accident-surface* (the commands a model types target `origin`), which is real but much smaller than claimed. |
| O8 `ext::` broker + `GIT_NAMESPACE` (lane 4 top pick) | **Fatal.** Namespace containment itself holds — but the identity is client-asserted: editing `.git/config` let agent1 create and delete a ref inside agent2's namespace. The seatbelt profile offered as proof began `(allow default)`, making its single `allow network-outbound` a no-op — the socket restriction was never actually tested. `protocol.ext.allow=always` re-enables `.gitmodules`-driven RCE. And unreviewed objects land in canonical before the panel runs. |
| "brehon already funnels every git call through one wrapper; fix is a `-c` prepend + 2 re-points" | **Broken.** 110 raw `Command::new("git")` in brehon-cli [v]; the agent-cwd exposure includes `merge`, `cherry-pick`, `commit`, `reset --hard` in epic.rs (:1213, :1231, :1267, :1279, :1316, :1461, :1462), not the two read-only sites named; and `filter.<name>.process` fires on `merge` and `cherry-pick` under the *complete* hardening vector. This is an architectural change to the integration flow, not a prepend. |
| "Thread the vector through `run_git` unconditionally" | **Broken.** `-c protocol.allow=never` kills every https/ssh remote; `-c core.hooksPath=/dev/null` disables `BREHON_PROTECTED_BRANCH_HOOKS` including the `reference-transaction` guard on the fetch that writes agent refs into canonical; `/dev/null` is `NUL` on Windows. Its `insteadOf` rationale was also wrong (client-side config). |
| "Refspec containment means integration survives untouched" | **Broken.** Contains *hostile escapes*; does not prevent *legitimate collisions*. Two agents naming `brehon/w1` force-overwrote each other in canonical; the loser's commit was gone after gc. Worktrees prevented this; clones do not. |
| "Agents sync with `git rebase origin/main`" (the headline ergonomic) | **Broken.** Rebase rewrites SHAs and the fetch-back is a force fetch → a commit no panel reviewed gets integrated with legitimate-looking `cherry-pick -x` provenance. |
| `gc.auto=0` as unconditional advice | **Inverted while hardlinked** — gc is what *de-aliases*. Correct only after `file://`. Order matters. |
| "core.pager fires on every git command; brehon is PTY-based" | **Wrong mechanism.** brehon-mcp pipes stdout (git_exec.rs:219); brehon-cli's TTY sites use canonical as cwd. Right conclusion, wrong reason — and the wrong reason invites `--no-pager` as the remedy for an index/worktree problem no flag fixes. |
| "Linux is cleaner for O4: bind-mount the socket" | **Inverted.** Landlock cannot restrict unix-socket connect until ABI v9 = Linux 7.1; no shipping LTS is there. O4 is the *least* portable option, not the most enforced. Landlock — never mentioned by two lanes — is the actual Linux answer for the path-based topologies, unprivileged from 5.13. |
| "confinement protects repos, not vendor state — acceptable" | **Broken.** It's not a visibility ceiling, it's an escape to the operator's unconfined shell via `~/.cargo/bin` on PATH and `~/.claude/settings.json` hooks. |
| "`chmod -R a-w base.git` protects it" | **False at one uid** (owner chmods back), and it breaks your own refresh. |
| O3 "clone from bundle has no HEAD / costs 701M" | **Overclaimed.** `--all` includes HEAD and clone checks out fine; bundle ≈ 21M. O3's real cost is bookkeeping. |
| "-800/-1200 lines, +80" | **Optimistic.** Deletion side is credible (agent_git_guard.rs is **438** lines [v], not 191; worktree.rs 1448 [v]; git_exec.rs 649 [v]). Re-baseline ≈ **-1000 / +300**. Still overwhelmingly a deletion. |

---

## 9. BUILD ORDER (delta against the prior recommendation)

Week-1 was: migrate worktrees → plain local clone; add Seatbelt. Both stand. Here's what moves.

**Changed (do these first — the first two are one-line fixes to live data-loss hazards in the already-blessed plan):**

| # | Change | Size |
|---|---|---|
| 1 | **`git clone file://…` everywhere, never `git clone <path>`.** Plus the spawn-time `nlink > 1` assertion. This is the whole hardlink class. | one word + one stat walk |
| 2 | **Namespace the fetch-back:** `'refs/heads/brehon/*:refs/heads/brehon/<agent-id>/*'`, no leading `+`, `--no-tags --no-recurse-submodules`, per-call timeout override. | one line |
| 3 | Seatbelt profile gains `(deny signal)(allow signal (target self)(target children))`, leaf-only cache grants, per-agent `TMPDIR`/`CLAUDE_CONFIG_DIR`/`CODEX_HOME`, realpath canonicalization. | small |
| 4 | "Never touch an agent repo" → **"inspect via libgit2, mutate only in canonical, Tier-B flags on the rest."** Re-point `git_patch_id` (git_ops.rs:606-608) and the `rev-list` at :649 at canonical after the fetch-back. Audit epic.rs's merge/cherry-pick/commit/reset sites — those are the un-hardenable ones and they must move into canonical. | the real work |

**Additive:**
5. `base.git` seed + narrow refresh + `BREHON_REFRESHED_AT` staleness signal in doctor (§6).
6. Tier A/B split in `run_git` (git_exec.rs:65); resolve `git` to an absolute path at startup.
7. Reviewed-tip-SHA gate on integration.
8. `confine_argv` + `probe()` compiler for the existing `SandboxSpec`; required `PtyConfig.confine`; delete `impl Default for PtyConfig`; Landlock backend with ABI ≥ 3 floor and non-async `confine` intercept.
9. Doctor checks: Landlock ABI + `unenforceable` list; cross-filesystem `worktree_root` (setup.rs `effective_worktree_root`) — with `file://` this is now only a *cost* warning, not a hazard; post-fetch object-count guard.

**Now unnecessary / do not build:**
- `extensions.preciousObjects` (already established).
- `agent_git_guard.rs` — 438 lines of PATH shim any agent bypasses with `/usr/bin/git`. Advisory, never a boundary. **Delete it only after real confinement lands**, not before.
- The bypass-lease machinery (`run_git_allow_protected_branch_commit`, `ProtectedBranchBypassLease`, `create_protected_branch_bypass_lease`, git_exec.rs:72-140) — once integration happens in canonical.
- Most of worktree.rs's registration bookkeeping (`find_worktree_name_by_path`, `scan_worktrees_dir_for_path`, `reconstruct_worktree_name`, stale-metadata recursion). A clone has no parent registration: removal is `rm -rf`, staleness is "directory missing". Keep the libgit2 functions — they're immune by construction. One exception in the same file: `git_path_has_tracked_files` (worktree.rs:804, :825) shells out and fires `core.fsmonitor`; move it to libgit2.
- The broker, the HTTP server, the sshd, git-daemon, `GIT_NAMESPACE`, a new sandbox policy type, a confinement trait, per-agent tokens or keys.

---

**Repro artifacts from this session:** `/tmp/gitchk`, `/tmp/gitchk2` (hardlink write-through, `file://` isolation, fetch-never-hardlinks, alias shadowing, `diff-tree` flag acceptance), `/tmp/pclone` + `/tmp/pagent` (pantheon timings). Lane artifacts: `/tmp/ref1/*.sh`, `/Users/recursive/.brehon-refute`.

---
title: The Brehon User Guide
author: Brehon contributors
status: draft
---

# The Brehon User Guide

> Or: *how I learned to stop worrying and convene a standing tribunal of language models to argue about my git diffs.*

This guide comes in two acts.

**Act I — Should You?** is for the person who found this repo, read the word
"panel," did some quick mental arithmetic about token pricing, and felt a cold
draft. Read it before you install anything. It will either talk you out of this
or make you dangerous.

**Act II — Okay, How** is for the person who read Act I, nodded grimly, and
typed `git clone` anyway. It takes you from an empty repo to a running bench of
agents building real software.

You can skip Act I. People skip the safety briefing on planes too. The exits
are still where the card says they are.

---

# Act I — Should You?

## What this actually is

Let's begin with the confession, because everything else makes more sense once
it's on the table:

This is software that uses AI to build software, built so that its author can
build *more* software. If you feel a small philosophical vertigo reading that
sentence, congratulations — your instincts are calibrated correctly. Somewhere
a few abstraction layers down there is presumably a human being who wanted to
write some code, and instead wrote a tool that orchestrates a panel of robots
to write the code, judged by a second panel of robots, refereed by a program
that watches the robots. The snake has not just eaten its tail; the snake has
filed the tail under `crates/` and added integration tests.

It is, to be clear, completely worth it. But you should know what you're
signing up for, and the honest pitch is not "this makes coding easy." The
honest pitch is: **some software is too big to hold in one head, or one context
window, and at that size the bottleneck stops being *writing* the code and
becomes *trusting* it.** Brehon is a machine for manufacturing trust at scale,
and trust, it turns out, is expensive. (You may have noticed the previous
section. There will be more sections like the previous section.)

### The one-paragraph version

Here's the whole thing in a breath, and then we'll slow down. You describe the
work as a **plan** — phases, epics, tasks, dependencies. Brehon hands ready
tasks to **workers**: AI coding agents, each in its own private git worktree so
they can't elbow each other. When a worker says "done," the work goes to a
**panel** of *other* AI agents who independently score it against a policy —
not one reviewer with three hats, three genuinely separate opinions. If the
panel's verdict clears the bar, the work merges into an epic branch (a staging
branch you inspect before it ever touches `main`). If not, it
goes back for another round, *to the same panel* — same roster of reviewer
lanes, bound to the task (whether they arrive remembering the last round or
freshly reset depends on a config knob we'll get to). Presiding over all of it
is the **supervisor**, which is really *two* things wearing one name: an AI lead
that plans and assigns the work, and a deterministic Rust loop that watches the
event store, tracks budgets, and nudges workers who've wandered into the woods —
calling on an AI itself only for the genuinely ambiguous judgment calls. That's
it. That's the machine. (If "the supervisor is two things" set off an alarm,
good; hold that thought, it has its own warning label later.)

### Why it's called Brehon

In early Irish law, a *brehon* (Old Irish *breithem*) was a professional judge.
Disputes weren't settled by one person's hunch — they went before judges who
weighed arguments, applied precedent, and issued binding verdicts. Crucially,
the *weight* of a verdict depended on the *standing* of the judge who gave it.
Sound familiar? It should: that's the reviewer panel, give or take fifteen
hundred years. Your code is the dispute. The panel is the bench. The policy
thresholds are precedent. The merge is the binding verdict. We did not invent
this idea; we just gave it a token budget and a terminal UI.

### The load-bearing honesty

Before you get attached: **this was built around one person's workflow, and
you are looking at the fifth internal rewrite of the idea.** It runs for *days*
on end, unattended, against real and genuinely gnarly software — a Rust telecom
packet-extraction engine, the kind of thing that emphatically does not fit in
one head. It plans its own follow-up work and keeps going, including
self-referential tasks where Brehon improves Brehon while the author sleeps. So
the long-horizon loop is battle-tested in the most honest way there is: by being
lived in. What is *not* promised is polish — this works for the author and will,
with near-total certainty, have bugs and rough edges for you. And the *shape* of
it — the lane model, the panel-judges-panel structure, the rhythm of a session —
was tuned to how one specific person works. It might fit you like a glove. It might be magnificent
overkill for what you're doing. It might be subtly, infuriatingly wrong for your
style in a way you only discover at round three of a review.

Treat this as a well-worn data point, not gospel. The design choices are the
interesting part; the claim that this is *the* way to do it is not being made.
With that established, and assuming the cost section didn't scare you off — read
on to find out whether you're the person this was built for.

## Who this is for (and who it isn't)

A tool that convenes a tribunal for every git diff is, let's say, *opinionated*
about when it's worth it. Most software does not need this. Saying so out loud
is the most useful thing this section can do, so here is the filter, with no
sales gloss.

### This is for you if...

- **You're building something genuinely large** — a system with real internal
  structure, dozens to hundreds of interlocking tasks, where the hard part isn't
  any single function but keeping the whole thing coherent while many changes
  land at once. The tell is the moment you catch yourself thinking "I can no
  longer hold all of this in my head," and you mean it, and it isn't even 3pm.
- **Correctness matters more than velocity.** You'd happily pay three frontier
  models to argue about a change if it means a race condition gets caught before
  it ships, not after it pages you at 4am with a name like `INCIDENT-0007`. The
  panel exists to find the bug you'd have caught in code review — on the day you
  didn't have time to do code review.
- **You can drive a CLI and you live there already.** Brehon is command-line and
  agent-CLI all the way down (more on that in a moment — it's not a preference,
  it's load-bearing). If your whole workflow is already a terminal and a
  multiplexer, this slots right in. If your workflow is a mouse, this is going to
  feel like emigrating to a country whose language is `--flags`.
- **You're comfortable with a meter running.** Not reckless — *comfortable*.
  You've read the cost section, you've set the budget caps, and the trade of
  *money for parallel, reviewed, trustworthy throughput* is one you want to make
  on purpose, with your eyes open and your thumb near the off switch.

### This is not for you if...

- **You want a faster autocomplete.** That already exists, it lives in your
  editor, and it costs a rounding error. Using Brehon to rename a variable is
  like chartering a container ship to mail a postcard. Technically the postcard
  arrives.
- **You're on a tight or fixed budget and need it to stay predictable.** The
  defaults are tuned for quality of verdict, the brakes ship disconnected (yes,
  still going on about that), and "I accidentally left it running" is a sentence
  that can have a price tag. You *can* cap it hard — but if the idea of an
  uncapped run makes your stomach drop, internalize that feeling before, not
  after.
- **The project is small enough to keep in your head.** If one person with one
  agent can hold the whole thing and ship it, the panel is pure overhead. The
  juice is in coordination *at scale*. Below that scale you're paying for
  ceremony.
- **You want a polished, stable product with a support line.** This is pre-1.0
  software built around one person's workflow. Crate boundaries move. Config
  shapes shift. The error messages occasionally assume you've read the source,
  because the author had.

### The CLI lock-in, stated plainly

One thing that is not a preference but a *premise*: Brehon drives real agent
**CLIs** — Claude Code, Codex, Gemini, OpenCode, Kimi, and friends — as
subprocesses, each one a worker or a reviewer on the bench. There is no hosted
dashboard, no web app, no "log in and watch it go" from your phone. The
orchestration *is* a coding session that happens to have five terminals in it,
and your seat is a terminal too. This is deliberate: the whole design assumes
long-horizon work driven from the command line, locked to the CLIs that do the
actual coding. If that sounds like a cage, it's the wrong tool. If it sounds
like Tuesday, welcome home.

If you read all of that and you're still here — you're the person this was built
for, or close enough that the differences will be educational. Act II is yours.

## How much is this going to cost me

You are reading the most important section in this document. If you read
nothing else, read this, then go change four numbers in your config before you
run anything.

Brehon spends money the way a panel of judges spends time: deliberately,
collectively, and more than you expected. Here is exactly where it goes, using
the configuration this very project ships with — because the honest version of
this section is just *reading our own config out loud and letting you hear how
it sounds.*

> **Read this before the scary numbers scare you off.** Everything below — three
> reviewers, five workers, four review rounds, which models on which lanes — is
> *one person's configuration*, not a law of nature. **You choose all of it.**
> Want a single reviewer and two cheap workers? That's a config edit. Want a
> seven-judge supreme court on a frontier-model bench? Also a config edit, and
> may God have mercy on your invoice. The author runs the setup you're about to
> see because it suits *their* work; your `brehon init` generates something far
> smaller — one *active* Claude worker judged by a single-member Claude panel,
> with lanes for every other agent CLI you have installed written in alongside it
> (inactive) so flipping one on is a one-line edit. The expensive defaults in
> this guide are an *illustration of the maximum*, not a starting requirement.
> Brehon is the machine; the dial settings are yours.

### The shape of the spend

There are four places tokens leave the building. None of them are bugs. All of
them are the entire point.

**0. The supervisor agent — the lead presiding over all of it.**

Before we even get to the workers, know that the thing coordinating them is
itself an AI. This repo runs the supervisor on Opus 4.6:

```yaml
roles:
  supervisor:
    name: claude-supervisor   # -> a lane running claude-opus-4-6
```

It plans, assigns, and untangles conflicts for the entire length of the
session. It is not the deterministic Rust loop people mean when they say "the
supervisor is free" — that's a *different thing that shares the name* (see the
mental-model section, where this trap is explained at length). The supervisor
*agent* is a premium seat that's clocked in the whole time. Budget for it.

**1. The worker pool — the agents that write the code.**

This repo's config runs **five workers at once**:

```yaml
roles:
  workers:
    - lane: claude-kimi-k2-6-worker   # cheap bulk lane
      min: 2
      max: 2
    - lane: minimax-token-plan-worker # structural / docs lane
      min: 1
      max: 1
    - lane: codex-worker-5-4          # the expensive one (gpt-5.4)
      min: 2
      max: 2
```

Five agents, each chewing through a task in its own git worktree, each holding
a context window full of your codebase. Two of them are running gpt-5.4 at
`xhigh` reasoning effort, which is the model equivalent of asking your most
expensive contractor to think *really hard* about everything, including the
typo fix.

**2. The reviewer panel — the agents that judge the code.**

This is the part that surprises people, so read it twice. Every time a worker
says "done," Brehon convenes a panel. Here is ours:

```yaml
review:
  policy:
    min_average_score: 8
    min_approvals: 3
    max_review_rounds: 4    # <- this number is a multiplier on your bill
  default_reviewers:
    - claude-reviewer          # Opus 4.6, correctness lens, high effort
    - codex-reviewer           # gpt-5.4, design lens, xhigh effort
    - deepseek-claude-reviewer # deepseek-v4-pro, performance lens, max effort
```

Three reviewers. Every one of them a frontier model running on high-to-maximum
reasoning effort. Each one independently reads the diff — up to
`max_diff_tokens: 12000` of it — plus whatever surrounding context it pulls in
to make sense of the change. They do not share work. That is the *design*: you
want three genuinely independent opinions, not one opinion echoed three times.
Three independent opinions cost roughly three times as much as one. This is not
a coincidence.

Now look at `max_review_rounds: 4`. If the panel asks for changes, the worker
revises, and the **same panel** reviews again. Up to four times. So a single
stubborn task can put your three frontier-model reviewers through their paces
*four rounds deep.*

**3. Research — the agents that read before anyone writes.**

Before a worker is even assigned a task, this config fires **three read-only
research briefs**:

```yaml
research:
  enabled: true
  attach:
    on_task_assignment: true
  routes:
    - jobs: [product-context, code-map, risk-brief]  # 3 jobs, every task
```

A spec brief, a code map, and a risk brief. Each is its own agent call. They
make the workers smarter and the reviews shorter. They are also three more
meters running before a single line of code exists.

### The arithmetic, made painfully concrete

Let's price one task that does not go smoothly — which, on hard software, is
most of them.

| Stage | Agent calls | Notes |
| ----- | ----------- | ----- |
| Research briefs | 3 | spec + code-map + risk, before assignment |
| Worker attempts | up to 4 | one per review round it has to redo |
| Reviewer panel | 3 × up to 4 | three reviewers, once per round |
| **Worst-case total** | **~19** | **per single task** |

Nineteen frontier-ish model invocations. For *one* task. And that's *on top of*
the supervisor agent, which is clocked in across the entire session presiding
over all of it — a per-session cost the per-task table doesn't even show. Now
open your plan document and count the tasks. A modest plan has thirty. A real one — the kind
that justifies owning a tool like this — has a couple hundred.

But notice what's actually driving that 19: *three* reviewers times *four*
rounds. Dial the same task down to one reviewer and a two-round cap and the
worst case falls to 3 research + 2 worker + (1 reviewer × 2 rounds) = **about
7** — a third of the spend, same machine, just different numbers in the same
config file. That's the whole point of the dials: the 19 isn't Brehon being
expensive, it's *this configuration* being thorough, and thorough is a setting.

This is not a warning that something might go wrong. This is the tool
**working exactly as designed**, on purpose, at full health. The expense isn't
a failure mode. It's the feature. You are buying three independent expert
opinions on every change, enforced by policy, and that is precisely the thing
that is expensive about it.

### About those brakes

Brehon ships with a budget system. Here is how it is configured in this repo:

```yaml
budget:
  max_total_cost: null        # no ceiling
  max_cost_per_task: null     # no per-task ceiling
  max_tokens_per_agent: null  # no per-agent ceiling
  alert_threshold_percent: 80 # warn at 80% of... null
  enforcement: Soft           # warn, never block
```

Every limit is `null`. Enforcement is `Soft`, which means even if you set a
limit, Brehon will *mention* that you blew past it rather than stop. The brakes
are installed. They are also, out of the box, disconnected. This is a defensible
default for the author, who watches the thing run and has made peace with the
bill. It is a terrible default for you on day one.

**Before your first real run, set these:**

```yaml
budget:
  max_total_cost: 25.00       # dollars, or whatever your config's cost unit is
  max_cost_per_task: 2.00
  enforcement: Hard           # actually stop, don't just sigh
```

Start low enough that a runaway loop wakes you up by *stopping*, not by
appearing on a statement at the end of the month.

### The four dials that change everything

If the table above frightened you — good, it should — these are the knobs that
turn the fear back into a number you chose:

1. **Panel size.** Three reviewers is the expensive heart of this. Drop
   `default_reviewers` to one or two for cheap work. You lose the independent
   cross-check; you keep your shirt. Reserve the full panel for code that can
   hurt you.
2. **`max_review_rounds`.** Four rounds is generous. Set it to `2`. A task that
   can't satisfy the panel in two rounds usually needs a human, not a third
   robotic opinion.
3. **Lane choice.** Two gpt-5.4 workers at `xhigh` is a luxury bulk lane. Route
   small and mechanical work to the cheap lanes (Kimi, MiniMax here) and reserve
   the expensive lane for the genuinely hard tasks. See *Turning the dials* in
   Act II for how routing does this automatically.
4. **Research.** Three briefs per task is thorough. If your tasks are small or
   your codebase is small, turn `on_task_assignment` off and let workers ask for
   research only when they actually need it (`worker_requests`).

Turn these down *before* you turn Brehon loose on a real epic. The defaults are
tuned for quality of verdict, not for the size of your bill. You have been told
twice now. There will not be a third time, because Act II assumes you listened.

---

# Act II — Okay, How

## Prerequisites & install

Good news: the install is boring. After Act I you've earned boring.

### What you need first

- **Rust 1.75+.** Get it from [rustup](https://rustup.rs/). Brehon is a Rust
  workspace and you're building it from source, so this is non-negotiable.
- **Git 2.x** with worktree support — i.e. any git from the last several years.
  Worktrees are how workers stay out of each other's way, so this matters more
  here than usual.
- **At least one agent CLI**, and realistically more than one if you want a
  panel with diverse opinions. Brehon speaks to Claude Code, Codex, Gemini CLI,
  OpenCode, Kimi, JetBrains Junie, and GitHub Copilot CLI, plus any
  OpenAI-compatible HTTP endpoint. You don't need all of them. You need the ones
  your config's lanes actually reference — install those and Brehon will find
  them on your `PATH`.
- **Zig 0.15.2+** — *only* if you're building the vendored ghostty
  terminal-emulation bindings from source. Most people never touch this;
  pre-built artifacts cover it. If you don't know whether you need Zig, you don't.
- **API access / auth** for whichever agent CLIs you chose. Each CLI handles its
  own credentials (an Anthropic key here, an OpenAI key there, an API base URL
  for the more exotic lanes). Brehon doesn't manage your keys; it just launches
  the CLIs that do. The practical test: run each agent CLI by itself once —
  `claude`, `codex`, whatever you're using — and make sure *it* can reach its
  model on its own (its own `login`, its own env vars). If the bare CLI works,
  Brehon can drive it; if it doesn't, Brehon can't fix that for you. Do this
  *before* your first run, because nothing deflates a dramatic first launch like
  five workers simultaneously discovering they're unauthenticated. (`brehon
  doctor`, coming up, will also flag this — but it's nicer to know first.)

### Build it

```bash
git clone https://github.com/VerifiedOrganic/brehon.git
cd brehon
git submodule update --init --recursive   # pulls in the vendored ghostty
cargo build --release
```

That produces `target/release/brehon`. Put it on your `PATH`:

```bash
install -m 0755 target/release/brehon ~/.local/bin/brehon
```

Confirm it's alive:

```bash
brehon --version
brehon doctor          # checks your CLIs, git, and config are all present and sane
```

`brehon doctor` is the unsung hero of this whole experience. Run it now, run it
whenever something feels off, and run it before you blame the orchestrator —
nine times out of ten the "bug" is a missing CLI or an unauthenticated lane, and
doctor will say so in plain language instead of making you read a stack trace.

## The mental model, properly

You met these words in passing during Act I. Now we'll make them load-bearing,
because the entire experience of using Brehon is just these nouns interacting.
Learn the nouns and the tool stops being mysterious. Skip them and every log
line reads like a ransom note.

- **Task** — the atom of work. Has a size (S/M/L), dependencies on other tasks,
  a "gate" condition that defines done (tests pass, etc.), and a status. Workers
  do tasks. Everything ladders up from here.
- **Epic** — a bundle of related tasks with its own integration branch (an
  `epic/*` branch; worker checkouts live under `brehon/`). Work merges into the
  epic branch, not straight into yours, so an epic is a staging area you can
  inspect before it touches `main`.
- **Initiative / Phase** — the higher tiers of a plan. An initiative contains
  epics; phases organize the plan document and map onto epics when you import it.
  These exist so a two-hundred-task project has *shape* instead of being a flat
  pile of TODOs.
- **Plan** — the structured document where you describe all of the above:
  phases, epics, tasks, dependencies, sizes, gates. This is your half of the
  bargain. Brehon dispatches what the plan describes; an empty plan dispatches
  nothing.
- **Worker** — an AI coding agent doing one task inside its own git worktree.
  Five of them run at once in this repo's config. They are the ones writing
  actual code.
- **Lane** — the part people trip on, so read slowly. A lane is a *named bundle*
  of: a launcher (which CLI), a model (which brain), a system prompt (what job
  it thinks it has), and reasoning effort. `codex-worker-5-4` is a lane. So is
  `claude-reviewer`. You assign roles to *lanes*, not to raw models — which is
  what lets you say "small tasks go to the cheap lane, scary tasks go to the
  expensive lane" in one line of routing config.
- **Launcher** — the boring layer under a lane: how to actually spawn a CLI
  (the command, its args, env vars). One launcher (`claude`) can back many lanes
  pointed at different models.
- **Panel** — the set of reviewer *lanes* bound to a task. Panel *affinity*
  means the same roster sticks with a task across revision rounds — so round two
  is judged by the same three lenses that judged round one. Whether those
  reviewers *remember* round one is a separate question, controlled by
  `review.lease_mode`, and it's worth knowing which mode you're in:
  - `exclusive` (the default): the panel holds its lease until the task is done.
    Same reviewers, retained context — genuine institutional memory. The robots
    remember exactly what they complained about and will notice if you ignored
    them.
  - `share_after_submit` (what *this repo's* config runs): after a reviewer
    submits, it's released and its session is hard-reset so it can go review
    something else. Same roster comes back next round, but **fresh** — no memory
    of the last round, judging your revision on its merits with a clean slate.
    You trade institutional memory for getting your expensive reviewers off the
    bench and back to work. On a busy board, that trade is often worth it.
- **Review round** — one full scoring pass by the panel. Policy caps how many
  rounds a task gets (`max_review_rounds`) before it escalates to a human, on the
  theory that if three frontier models can't be satisfied in N tries, the problem
  is upstream of the robots.
- **Verdict / score / finding** — what a reviewer produces: a numeric score
  (1–10), an overall verdict, and structured findings tagged blocking /
  suggestion / nitpick with file and line. The score collector adds these up
  against the policy thresholds to decide merge-or-redo.
- **Worktree** — the isolated git checkout each worker gets, parked *outside*
  your repo under a platform data directory, scoped as `{repo-name}-{short-hash}`.
  This buys you *live* isolation: five agents editing "the same project" can't
  clobber each other's uncommitted work, because they're literally in five
  different directories. What it does **not** buy you is conflict-free
  integration. When finished work is cherry-picked onto the epic branch, two
  workers who touched the same lines will still conflict — Brehon previews this
  (`preview_conflicts`) and the supervisor owns resolving it. This is exactly
  why a good plan keeps tasks on *disjoint write scopes*: isolation stops the
  live collisions, but only careful task design stops the merge-time ones.
- **Event store** — the append-only source of truth. Every meaningful thing that
  happens (task assigned, review scored, worker spawned) is an immutable event on
  disk. Brehon's state isn't a guess reconstructed from process memory; it's a
  ledger. This is what makes it restart-safe — kill it mid-session and it rebuilds
  from the events.
- **Supervisor** — here's a wart you need up front, because it bit the person
  writing this guide: *"supervisor" names two different things.* There is the
  **supervisor agent** — a genuine AI (this repo runs Opus 4.6 on it) with a
  system prompt that reads "You are a senior engineering lead." It plans work,
  assigns tasks, guides workers, and untangles integration conflicts the task
  graph can't resolve on its own. It is assigned in `roles.supervisor`, it costs
  tokens like any other mind, and it is unmistakably an AI. Then there is the
  **supervision loop** — `brehon-supervisor`, a deterministic Rust process that
  streams the event store, flags workers who've gone quiet, tracks budgets, and
  fires nudges. *Mostly* free... except it, too, calls an AI for the genuinely
  ambiguous judgment calls ("is this worker stuck or just thinking hard? should I
  extend its budget?"). So the clean story "the supervisor is free deterministic
  Rust" is a half-truth that sounds great in a README and falls apart the moment
  you look at the config. Hold both meanings in your head; the logs use the word
  for both.
- **Orchestrator** — the genuinely deterministic dispatcher. Computes the task
  DAG, figures out what's ready (dependencies met), and assigns ready tasks to
  free workers by lane. This one really is plumbing, no asterisk.

If you remember only one distinction from this list, make it this one:
**minds cost tokens; plumbing doesn't.** Workers, reviewers, the supervisor
*agent*, research, advisors — anything that has to exercise judgment is an AI,
and it bills. The orchestrator's dispatch, the event store, and the
bookkeeping inside the supervision loop are deterministic code, and they're
free. The whole architecture is organized around spending money only where a
judgment call genuinely requires a mind, and using boring deterministic logic
for everything else. The thing to *un*learn is the tidy slogan that "the
supervisor is the free part" — the supervision *loop* is mostly free, but the
supervisor *agent* is one of your priciest seats. Once you hold that
distinction, every design choice in Brehon stops looking arbitrary and starts
looking like the same decision made over and over: *is this judgment, or is
this bookkeeping?*

## Your first run

We're going to get a single task through the machine end to end. Not a
two-hundred-task epic — one task, so you can watch every stage happen and,
crucially, see the meter move *before* it moves a lot. Think of this as the
test flight where we keep one hand on the budget the whole time.

### Step 1 — Initialize

From the root of a real git repo (use a scratch project for your first run;
resist the urge to point this at production on day one):

```bash
brehon init
```

This detects which agent CLIs you have on your `PATH` and writes a starter
`.brehon/config.yaml` wired to *those* agents — so the config you get is shaped
by what you actually have installed, not a fantasy. It also adds Brehon's
entries to `.gitignore` so you don't accidentally commit a worktree. On success
it prints a Quick Start and a Next steps block that looks roughly like:

```
✓ Created .brehon/
✓ Created .brehon/config.yaml
✓ Updated .gitignore

Next steps
  1. Edit .brehon/config.yaml to configure agents and roles
  2. Run brehon doctor to verify your setup
  3. Run brehon to start orchestrating
```

It is telling you the next two steps. We are going to do the next two steps.
This is the rare moment where the tool and the tutorial agree completely.

### Step 2 — Doctor

```bash
brehon doctor
```

Run this *now*, before anything spends a token. Doctor checks that the CLIs your
config references are actually installed and reachable, that git is sane, and
that your config parses. If a lane points at `codex` and you don't have Codex
installed, this is where you find out — calmly, in plain text — instead of
discovering it three workers deep into a launch. If doctor is unhappy, fix what
it names and run it again until it's quiet. A quiet doctor is a happy doctor.

### Step 3 — Set a budget cap (yes, even now)

Open `.brehon/config.yaml` and find the `budget` block. Before your first real
run, give it a hard ceiling, because the starter config — like every Brehon
config — does not assume you want one:

```yaml
budget:
  max_total_cost: 5.00      # tiny on purpose; this is a test flight
  max_cost_per_task: 2.00
  enforcement: Hard         # actually stop, don't just sigh and continue
```

Five units. If a single task somehow blows through five units, you want it to
*stop and tell you*, because something is wrong and you'd rather learn that now,
for five units, than later, for considerably more. You can raise this the moment
you trust the thing. You will trust it faster if your first run can't surprise
you.

### Step 4 — Give it exactly one task

You don't need a whole plan document to start. Create a single task directly.
Here's a real, small, self-contained one from siftcap's actual world — a
malformed-packet case, which is exactly the kind of bounded, well-scoped work
that makes a good test flight (swap in whatever's true for *your* project):

```bash
brehon task create \
  --title "Reject truncated Diameter AVP without panicking" \
  --task-type task \
  --acceptance "decoder returns a bounded error on a truncated AVP, never panics" \
  --acceptance "a corpus fixture for the truncated packet is added" \
  --acceptance "a test asserts the error path"
```

Pick something genuinely small and self-contained for this — a task a competent
human would finish before their coffee got cold. The point of the first run
isn't the feature; it's watching the *machine*, and a small task keeps the
watching cheap. (A bonus of choosing something like the above: a clean error
path on bad input is the kind of change where you'll actually *see* the
correctness reviewer earn its keep.)

### Step 5 — Run it

```bash
brehon
```

Bare `brehon` is the same as `brehon run` — it's the default. A TUI dashboard
takes over your terminal. Don't panic at the number of panes. Here's what you're
looking at, in brief (the next section breaks it down properly):

- **Worker panes** — your worker agents, live terminals, picking up the task and
  writing code in their isolated worktrees.
- **The supervisor pane** (right-hand side) — the AI lead's view: assignments,
  nudges, budget state.
- **Reviewer panes** — quiet for now; they wake up when a worker reports the task
  ready, at which point your panel convenes and the interesting (and pricey)
  part begins.

To quit, press **Ctrl+Q** (or **Ctrl+\\**). Navigate panes with **Tab** and the
arrow keys. That's enough to survive your first session; the full controls are in
the dashboard section.

### What you just watched

In the space of one small task, the whole machine ran: a worker took an
assignment in its own worktree, implemented it, and reported ready; a panel of
reviewers independently scored it against the policy; the score collector
checked the thresholds; and either the work cleared the bar and was integrated
onto an epic branch, or it bounced back for another round. The supervisor
presided over all of it, and the budget cap sat there the whole time as your
dead-man's switch.

Now you've seen the loop. Everything else in this guide is about pointing that
same loop at work big enough to justify it — and turning the dials so it costs
what you meant it to.

## Writing a plan that doesn't fight you

One task was the test flight. Real work arrives as a **plan** — and the plan is
your half of the bargain. Brehon dispatches what the plan describes; a vague
plan produces vague work, expensively. This is the highest-leverage hour you'll
spend, so spend it.

### Two commands, one good reason

Plan ingestion is split across two commands that share extraction logic but
differ in what they do with the result:

- **`brehon extract-plan FILE`** — turns a plan document into normalized JSON.
  Prints to stdout, or `--output PATH`. **Does not touch the board.**
- **`brehon import-plan FILE`** — takes a plan (the source doc *or* extracted
  JSON) and builds the `initiative → epics → tasks` tree on the board, including
  a final hardening epic. `--dry-run` previews without writing.

The split exists for one reason, and it's a money reason — see the next bit.

### The three extraction modes (one of them is the expensive one)

| Mode | What it does | Cost |
| ---- | ------------ | ---- |
| `direct` | Parses your markdown with the built-in deterministic parser. | **Free, instant.** |
| `supervisor` | Feeds the doc to your supervisor lane's CLI under a JSON schema and lets an LLM normalize it. | **Expensive** — at minimum one model call, often one *per phase or per task*. |
| `auto` (default) | Tries `direct`, falls back to `supervisor` only if parsing fails. | Free if your doc parses; pay only when it doesn't. |

`supervisor` mode is a genuine on-ramp cost and people forget it because it
happens once, quietly, before the fun starts. If your plan already follows the
structure below, use `direct` and pay nothing. If it's free-form prose, you'll
pay an LLM to read the whole thing and turn it into structure — once.

### The shape the direct parser wants

Hit this structure and you skip the LLM entirely. Phases contain epics; epics
contain a table of tasks:

```markdown
# My Plan Title

## Phase 0: Foundation

### Epic 0.1: Storage layer

| ID    | Task                  | Deps   | Size | Gate          | Status |
| ----- | --------------------- | ------ | ---- | ------------- | ------ |
| 0.1.1 | Define event types    |        | S    | unit tests    | Open   |
| 0.1.2 | Implement append path | 0.1.1  | M    | smoke test    | Open   |

### Phase 0 Gate

| ID  | Task                     | Deps          | Size | Gate     | Status |
| --- | ------------------------ | ------------- | ---- | -------- | ------ |
| 0.G | Phase 0 integration test | 0.1.1, 0.1.2  | L    | all pass | Open   |
```

The columns that matter: **Deps** builds the dependency DAG (Brehon won't
dispatch a task until its deps are done), **Size** (S/M/L) feeds routing —
small work to cheap lanes, large work to expensive ones — and **Gate** is your
definition of done. (See `crates/brehon-cli/src/commands/import_plan/tests.rs`
for canonical examples if you want to match the parser exactly.)

### The pattern that saves money: extract once, import many

```bash
# One expensive LLM pass to normalize a free-form plan into JSON.
brehon extract-plan PLAN.md --mode supervisor --output .brehon/plan.json

# Hand-edit .brehon/plan.json if you want — it's a checked-in artifact.

# Cheap, deterministic, re-runnable. Re-import as often as you like.
brehon import-plan .brehon/plan.json
```

The normalized JSON is a *cache*. Extraction over an LLM can take minutes and
cost real money; importing from the cached JSON is free and instant. Pay the
extraction toll once, then iterate against the JSON forever. This is the whole
reason the two commands are separate, and now you know it.

### Write your plan for disjoint write scopes

Remember from the mental model: isolated worktrees stop *live* collisions, but
two tasks that edit the same files still **conflict at integration**, and
resolving that falls to the supervisor (read: more tokens, more wall-clock).
The fix isn't a setting — it's plan design. Carve tasks so that concurrently
runnable ones touch *different* files. Use `Deps` to serialize the ones that
genuinely can't be parallel. A plan whose parallel tasks have disjoint write
scopes runs smoothly and cheaply; a plan where five workers all want to edit
`mod.rs` produces a merge negotiation you're paying frontier models to conduct.
The plan is where you prevent that, or don't.

## Reading the dashboard: what the panel is doing

When you run `brehon`, a TUI takes over your terminal. It looks busy because it
*is* busy — there are multiple live agent terminals on screen at once. Here's
how to read it without feeling like you've wandered onto someone else's flight
deck.

### The layout

The screen splits left and right. The **left** side is a set of tabs:

- **Dashboard** — the overview: what's happening across the whole session.
- **Workers** — one sub-tab per worker, each a live terminal you can watch
  write code in real time. This is the hypnotic one.
- **Reviewers** — the panel's terminals, grouped by panel. Quiet until a task
  hits review, then suddenly very much not quiet.
- **Advisors** / **Research** — the optional read-only rooms (brainstorming and
  research briefs), when you've got them enabled.
- **Runtime** — runtime state and events.

The **right** side (about 40% of the width) is always the **supervisor pane** —
the AI lead's view: assignments going out, nudges to stuck workers, budget
state ticking along. If you want to know *why* the system is doing what it's
doing, the supervisor pane is where it narrates itself.

### Controls (the honest version)

You do not need to memorize a binding table, because the app ships one:

- **`?`** — open the keybinding overlay. This is the source of truth for every
  control, it's always one keypress away, and it will never be out of date the
  way a table in a guide eventually is. When in doubt, press `?`.
- **`Ctrl+Q`** (or **`Ctrl+\`**) — quit the session.
- **`Tab`** and the **arrow keys** — move between tabs and panes.

That's the survival kit. Everything else is in the `?` overlay.

### Reading a verdict

The moment that matters is when a worker reports a task ready and the panel
returns its scores. Each reviewer produces three things: a **score** (1–10), a
**verdict** (approved / changes-requested / rejected), and zero or more
**findings** tagged *blocking*, *suggestion*, or *nitpick* with a file and line.

The score collector then applies your policy. With this repo's thresholds, a
task merges only if the panel hits an **average ≥ 8**, **every individual score
≥ 7**, **at least 3 approvals**, and **no unresolved blocking findings**. Miss
any one of those and the task goes to `ChangesRequested`, the worker revises,
and the panel reviews again — up to `max_review_rounds` times before it gives
up and escalates to you.

So when you see a task bounce, read the *findings*, not just the score. The
findings are where three frontier models just told you, with file and line
numbers, exactly what they think is wrong. That feedback is the product you
paid for. Reading it is how you get your money's worth — and how you learn
whether the panel is catching real problems or just being fussy, which tells
you whether to tighten or loosen the dials in the next section.

## Turning the dials

The cost section told you *that* the dials exist. This is *which* dial does
what. They all live in `.brehon/config.yaml`, and they sort cleanly by what
you're trying to change. The author's running config has these tuned for
maximum verdict quality with the budget brakes off — your job is to find your
own point on the same dials.

### "I want it cheaper"

- **Shrink the panel.** Three reviewers is the single biggest line item. Drop
  `review.default_reviewers` to two, or one, for low-risk work. You lose
  cross-checking; you keep money. Reserve the full bench for code that can hurt
  you.
- **Cut the rounds.** `review.policy.max_review_rounds: 2` instead of `4`. If
  three frontier models can't be satisfied in two passes, the problem usually
  wants a human, not a third robot round.
- **Route work to cheap lanes.** This is the big structural lever. The `routing`
  block sends tasks to lanes by content and size: small/mechanical to the cheap
  lane, large/high-risk to the expensive one. The author's config routes `S`
  tasks to Kimi, `M` to MiniMax, and only `L`/risky work to gpt-5.4 — so the
  pricey model only shows up when it's earning it.

  ```yaml
  routing:
    default_worker_lane: claude-kimi-k2-6-worker   # cheap by default
    escalation_lane: codex-worker-5-4              # expensive, only when needed
  ```
- **Trim research.** Three briefs per task is thorough. Set
  `research.attach.on_task_assignment: false` and let workers pull research only
  when they ask (`research.worker_requests`), instead of pre-paying for a code
  map on the one task that turned out to be a two-line change.
- **Compress context.** `context.compression` can shrink model-facing context
  before it's sent. It's off by default and **fail-closed** — if the compressor
  is missing, errors, or doesn't actually save tokens, Brehon sends the
  original. Low-risk to try.

### "I want it safer" (the brakes)

- **`budget`** is the seatbelt. Set `max_total_cost` and `max_cost_per_task`,
  and set `enforcement: Hard` so it *stops* rather than merely tutting at you.
  `alert_threshold_percent` warns you on the way up. We have now mentioned this
  enough times that it should be muscle memory.
- **Stuck detection.** `supervisor.stuck_detection` decides when a quiet worker
  is "stuck" vs "thinking," and `supervisor.nudge` controls how soon it gets a
  soft nudge vs real guidance. Tighten these if workers wander off; loosen them
  if your supervisor keeps poking an agent that was, in fact, just compiling.
- **Escalation.** `escalation.human_in_loop: true` keeps *you* as the final
  backstop when the robots are out of moves. Leave this on until you have a very
  good reason not to.

### "I want it faster / more parallel"

- **`orchestration.max_active_workers` / `spawn_workers`** set how many workers
  run at once. More workers = more throughput = more concurrent spend, and more
  chances for two of them to want the same file (see: disjoint write scopes).
  Five is the author's number; yours depends on your machine, your wallet, and
  how cleanly your plan parallelizes.
- **`review.lease_mode: share_after_submit`** frees a reviewer the moment it
  submits so it can go judge another task, instead of being pinned to one task
  until that task is done (`exclusive`). On a busy board this keeps your
  expensive reviewers working instead of waiting — at the cost of the
  cross-round memory described back in the mental model. It's the right trade
  more often than not, which is why this repo runs it.

### "I want better verdicts"

- **Diversify the panel's lenses.** The author's three reviewers aren't three
  copies of one prompt — each has a different `system_prompt`: one reviews for
  **correctness and safety**, one for **design and specification**, one for
  **performance and resource safety**. Three angles catch what three identical
  reviewers would all miss together. If you're going to pay for three opinions,
  make them three *different* opinions.
- **Raise the bar.** `min_average_score`, `min_individual_score`, and
  `min_approvals` are how strict "good enough" is. Raise them for code that
  ships to users; relax them for an internal spike. For full-council panels,
  keep `min_approvals` equal to the panel size because Brehon requires every
  seated reviewer to approve. Just know that a higher bar means more rounds,
  and more rounds mean — say it with me — more tokens.

The through-line: every quality dial and every cost dial are *the same dials*,
turned in opposite directions. There is no setting that gives you more rigor for
less money, because the rigor *is* the money. What the config gives you is the
steering wheel. Where you point it is the actual skill.

## When it goes sideways

It will, occasionally, go sideways — five agents, real money, and live git is an
exciting combination. The good news is that Brehon is event-sourced and built to
be interrupted, so "sideways" almost never means "lost." Here's your kit.

### First, always: `brehon doctor`

```bash
brehon doctor            # read-only diagnosis
brehon doctor --repair   # attempt fixes
brehon doctor --json     # machine-readable, for scripts
```

Nine times out of ten the "bug" is a missing CLI, an unauthenticated lane, or a
malformed config, and doctor will say so in plain language. Run it *before* you
suspect anything cleverer. `--repair` fixes what it safely can; `--json` is there
when you want to wire it into something.

### Something's stuck

A worker that's gone quiet isn't necessarily dead — the supervisor's stuck
detection is *operation-aware*, so it tries to tell "thinking hard" from "wedged"
before it nudges. But if you can see a worker is genuinely stuck:

- Watch its pane (Workers tab) — the live terminal usually shows you what it's
  staring at.
- The supervisor will nudge it on the configured timers (`supervisor.nudge`),
  and escalate to you if nudging doesn't help.
- For process-level inspection, `brehon ps` shows in-flight runs and
  `brehon kill` stops them.

### Cleaning up — three tools, escalating in violence

This is the part to read carefully, because the three commands are *not*
interchangeable and the difference is how much they delete:

- **`brehon maintenance`** — the gentle one. **Reports** stale worktrees and
  branches by default; only deletes with `--prune` (and confirms first, unless
  `--force`). `--json` for a machine-readable report. Start here.
- **`brehon reset`** — the middle one. Clears **runtime state and worktrees**
  but **preserves your `.brehon/config.yaml` and authored content** (rules,
  memories, skills). Use it to start a session fresh without re-initializing.
- **`brehon clean`** — the nuclear one. Removes the **entire `.brehon/`
  directory** and all Brehon artifacts. This is "uninstall from this repo."
  You'll run `brehon init` again after.

All three share a hard guardrail: the safety check (`is_safe_brehon_branch`)
**refuses to touch protected branches** — `main`, `master`, `develop`, `trunk`,
`HEAD`, the current epic branch, or anything not under the `brehon/` prefix. So
even the nuclear option will not delete your `main` branch. This guard is not a
suggestion; it's load-bearing, and it has saved the author from the author.

### After a crash

You generally don't have to do anything special. The event store is the single
source of truth, and on the next `brehon run` Brehon rebuilds: it replays events
to reconstruct in-memory state, re-derives review-panel state, checks every
worktree for stale lockfiles / half-finished rebases / partial cherry-picks and
either repairs or flags them, and re-evaluates each task against the recovered
worker pool to decide resume-vs-retry-vs-fail. Kill it mid-session and it picks
up where it left off, because it was never trusting process memory in the first
place. This is the entire reason it's event-sourced, and the day it saves you,
you'll forgive it for everything else.

## Where to go next

You now have the whole loop: what Brehon is, who it's for, what it costs, how to
install it, the vocabulary, a first run, how to write a plan, how to read the
dashboard, which dials to turn, and what to do when it bucks. That's enough to
run real work. When you want to go deeper:

- **[docs/ARCHITECTURE.md](ARCHITECTURE.md)** — the full technical walkthrough:
  startup flow, the event store, the supervision loop, the review engine,
  recovery, the MCP server, and the TUI. Read it when you want to know *how*,
  not just *which knob*.
- **[docs/adr/](adr/)** — the Architecture Decision Records. Each one explains
  *why* a major choice was made: why Rust, why a deterministic supervision loop,
  why reviewer panels, why git worktrees, why fjall and tantivy, why local-first.
  This is where the reasoning lives, including the roads not taken.
- **The config schema** — `crates/brehon-config/src/` (defaults and the
  validator) is the authoritative source for every setting. When this guide and
  the schema disagree, the schema wins; tell someone so the guide gets fixed.
- **`brehon <command> --help`** — every subcommand's full flag set. The guide
  covers the path most travelled; `--help` covers the rest.

One closing note, because it's the honest one. This guide documents the loop
using the configuration the author actually runs — a particular panel size, a
particular set of lanes, particular thresholds. **None of that is Brehon.**
Brehon is the loop; the numbers are just one person's answer to "how much rigor,
at what price." Your answer will be different, and finding it — task by task,
bill by bill — is the actual craft here. The tool convenes the tribunal. What
you put in front of it, and how strict a bench you can afford, is up to you.

Now go build something too big to hold in your head. That's the only kind of
thing worth all this.

# Running Brehon on local models

Brehon can drive a panel of **locally-hosted** models — worker, reviewers, and
supervisor all running on your own hardware, no paid API. It talks to any
**OpenAI-compatible** HTTP endpoint, which is what every common local server
speaks (llama.cpp's `llama-server`, `llama-swap`, Ollama, LM Studio, vLLM).

This guide is honest about the one hard constraint and how Brehon works around
it. Read it before you point Brehon at a local box for a long run.

## The one constraint that matters

A single local inference server holds **one model in VRAM at a time** and has a
**fixed number of request slots**. Brehon's whole design is the opposite: a
supervisor, several workers, and a *panel* of reviewers, each an independent
client, and a review fans out to every reviewer at once.

If you want **different models per role** (the point of a judge panel — diverse
opinions) but your GPU only holds **one model at a time**, then switching models
is physically unavoidable. A 3-reviewer panel on 3 different models means 3
sequential model loads. No orchestrator can dodge that; it can only make the
swaps **orderly instead of thrashing**, and be honest that it's slow.

Brehon does two things about this:

1. **Per-endpoint serialization** (`max_concurrency`). Every lane that points at
   the same `base_url` shares one concurrency budget. With `max_concurrency: 1`,
   Brehon sends one request at a time to your server and *queues* the rest
   (they're durable on disk and retried), instead of stampeding a single-slot
   server or interleaving requests that force a reload on every turn.
2. **Context-window-aware trimming** (`context_window`). Local models have small
   context windows and `llama-server` *errors* on overflow by default (it does
   not silently truncate). Tell Brehon the window and it trims history to fit.

> **If you only take one thing from this page:** for a single-GPU box, set
> `max_concurrency: 1` and `context_window` on your local launcher, and keep the
> panel small. See ["Easy mode"](#easy-mode-one-model-zero-swaps) to avoid swaps
> entirely.

## Serving stack: llama-swap (recommended)

[`llama-swap`](https://github.com/mostlygeek/llama-swap) is a small proxy that
puts **one** OpenAI-compatible endpoint in front of `llama.cpp`, loading the
right model on demand by the request's `model` field. Brehon points every local
lane at that one endpoint and selects the per-role model by name; llama-swap
handles the load/unload, and its `groups` let you decide which models may
coexist vs. swap.

Minimal `llama-swap.yaml` (three role models, swapped as needed on one GPU):

```yaml
models:
  qwen3-coder-30b:        # worker
    cmd: >
      llama-server --port ${PORT} --jinja
      -m /models/Qwen3-Coder-30B-A3B-Instruct-Q5_K_M.gguf
      -c 32768 -ngl 99
  devstral-24b:           # reviewer A
    cmd: >
      llama-server --port ${PORT} --jinja
      -m /models/Devstral-Small-2-Q5_K_M.gguf
      -c 32768 -ngl 99
  gpt-oss-20b:            # reviewer B / supervisor
    cmd: >
      llama-server --port ${PORT} --jinja
      -m /models/gpt-oss-20b-Q5_K_M.gguf
      -c 32768 -ngl 99
```

Start it: `llama-swap --config llama-swap.yaml --listen 127.0.0.1:8080`.
Brehon's `base_url` is then `http://127.0.0.1:8080/v1`.

### llama-server flags that matter

- **`--jinja` is required for tool calling.** Without it the model's tool calls
  leak out as plain text and Brehon's agents can't act. This is the single most
  common "my local model does nothing" cause.
- **`-c / --ctx-size`** sets the context window. With multiple slots (`-np N`)
  the window is *divided* across slots, so the per-request window is
  `ctx_size / N`. Set Brehon's `context_window` to that per-request value, not
  the total. (Check `GET {base_url}/props` → `default_generation_settings.n_ctx`
  for the real number.)
- **Avoid sub-Q5 quants for agents.** Q2/Q3/IQ quants reliably produce malformed
  tool-call JSON. Prefer `Q5_K_M`/`Q6_K` or better for tool-using lanes.

## Model choice

Tool-calling reliability matters far more than parameter count for agentic
coding — a well-trained 14–30B tool-caller beats a poorly-suited 70B. Known-good
open-weight tool callers (verify against current model cards):

- **Qwen3-Coder-30B-A3B-Instruct** — MoE, ~3B active, strong agentic coding.
- **Devstral Small (24B)** — purpose-built for tool-use coding harnesses.
- **gpt-oss-20b** — Apache-2.0, 128k context, good function calling. (Wants
  `temperature: 1.0`, `top_p: 1.0`, and repetition penalty *off* — set via
  `extra_body`.)

Treat Llama-3.3-70B as a poor agentic default; it benchmarks weakly on tool
calling despite its size.

## Brehon config

See [`brehon-local-config.yaml`](brehon-local-config.yaml) for a complete,
copy-pasteable example. The shape:

```yaml
launchers:
  local:
    adapter: NativeAgent          # Brehon's first-party OpenAI-compatible agent
    provider: openai-compatible
    base_url: http://127.0.0.1:8080/v1
    # api_key_env: LLAMA_API_KEY  # only if you started the server with --api-key
    max_concurrency: 1            # one in-flight request to this endpoint
    context_window: 32768         # per-request window; trims history to fit
```

Every local lane uses this one launcher and picks its model by name. The lane's
`model.name` **must match the llama-swap model id**:

```yaml
lanes:
  local-worker:
    launcher: local
    model: { provider: local, name: qwen3-coder-30b }
  local-reviewer-a:
    launcher: local
    model: { provider: local, name: devstral-24b }
  local-supervisor:
    launcher: local
    model: { provider: local, name: gpt-oss-20b }
```

Because all three lanes share one `base_url`, `max_concurrency: 1` serializes
them onto the single GPU automatically — no per-lane coordination needed.

### Knobs

| Setting | Where | Why for local |
|---|---|---|
| `max_concurrency` | launcher | Serialize lanes onto one server's slots. `1` for single-GPU. |
| `context_window` | launcher | Trim history to the model's window; avoids llama.cpp's hard overflow error. |
| `BREHON_AGENT_MAX_TOOL_ROUNDS` | launcher `env:` | Hard ceiling on tool-call rounds so a weak model can't loop forever. |
| `extra_body` | launcher | Inject `temperature`, `response_format`/`grammar`, etc. into each request. |
| `budget.max_wall_clock_minutes` | config | The meaningful budget locally — dollar cost is ~$0; cap **time**. |

Example launcher with the extra guardrails:

```yaml
launchers:
  local:
    adapter: NativeAgent
    provider: openai-compatible
    base_url: http://127.0.0.1:8080/v1
    max_concurrency: 1
    context_window: 32768
    env:
      BREHON_AGENT_MAX_TOOL_ROUNDS: "40"   # fail a turn that won't stop calling tools
    extra_body:
      temperature: 0.2                     # lower temp = fewer malformed tool calls
```

To force schema-valid tool JSON on a flaky model, add a grammar/response_format
through `extra_body` (llama.cpp constrains decoding to it):

```yaml
    extra_body:
      response_format: { type: json_object }
```

## Easy mode: one model, zero swaps

Diverse models on one GPU is slow (a swap per reviewer). If you'd rather have a
fast, multi-perspective panel without swaps, run **one** local model for every
role and differentiate reviewers by **persona** (different `system_prompt`)
instead of different weights. You lose true weight-diversity but keep
independent opinions, and there are **no model swaps** at all. This is the
recommended default for a single-GPU box — see the "easy mode" block in
[`brehon-local-config.yaml`](brehon-local-config.yaml).

## Cost and budgets

Local inference is ~free per token, so the dollar caps in `budget` don't bind.
What *does* bind is **time**: set `budget.max_wall_clock_minutes` so an
unattended run can't grind for days on slow local inference. Token caps still
work as a coarse throughput guard.

## What's NOT handled / honest caveats

- **Swaps are slow.** A diverse panel on one GPU reloads models between
  reviewers (~5–30s each). Keep panels to 2 reviewers, or use easy mode.
- **No live capability negotiation.** Brehon assumes the endpoint supports tools;
  if you forget `--jinja`, agents will stall. `brehon doctor` probes the endpoint
  (see below) but cannot fully validate tool-calling quality.
- **Context trimming is approximate.** Brehon uses a conservative token estimate
  (it over-counts to stay safe). For very tight windows, lower `context_window`
  further or shrink your panel/system prompts.
- **llama.cpp moves fast.** Flag names and defaults change between builds; pin a
  version and re-check `--jinja`/`--ctx-size`/`-np` against it.

## Verifying your setup

```sh
brehon config validate     # checks the config parses and lanes resolve
brehon doctor              # probes configured local endpoints (up? which model? context?)
```

`brehon doctor` will report, for each OpenAI-compatible launcher (`adapter:
OpenAiCompatible` or `NativeAgent`) with a `base_url`, whether the endpoint
answers and which model(s) it advertises — a fast way to catch "server isn't
running" or "wrong port" before starting a run.

# Strata models in Golem (planned)

Status: **design, not built** (2026-10-20). Blocked on the engine viability probe below;
nothing here changes the defaults until Strata passes it.

**The probe ran on 2026-10-20 and Strata refuses.** v0.1.45 was started at the preset's
236544 context (`engines/strata/engine.ini`'s `strata_context`, applied to the installed
`strata-qwen36-ud-q4_k_xl.json`) and its engine exited before it was ready:

```
strata-q36: context 236544 cells, prompt chunk 2048, MTP block loaded, 0.00 GiB of VRAM free
strata-q36: not enough VRAM left for the hybrid experts' buffers and slots
            (1256 MiB needed, 0 MiB free after the weights, the context and the sessions)
```

The context alone takes the card: its KV is ~57 KB per token against llama.cpp's ~12.7 KB,
so 236544 needs ~13.5 GB before any weights. That is not a placement problem to tune around
- no expert split makes 236544 fit on 12 GB - so this is a refusal, not a slow start. It is
recorded as a `reject` row in `bench/results.jsonl` with the engine's own words in the note,
and the installed config was put back to the 32768 that does run. Per the rule below,
nothing on this page ships: Golem keeps llama.cpp and this waits for 0.2.x.

**The adapter and the machine now disagree on purpose.** `strata_context = 236544` in
`engines/strata/engine.ini` is what a fresh install would ask for, and this box cannot serve
it. A Strata install on a 16 GB or 24 GB card would want it; on 12 GB the number is
aspirational. Whichever way that is resolved - a smaller adapter default, or a fit check
before the context is written - it is part of the design below, not a one-line change.

## What we want

Two things, both user-facing:

1. **Any Strata model that fits the machine should be one command away.** Golem lists the
   models Strata can serve, checks which fit this box (VRAM wall + system RAM), and offers
   them as presets: pull, bench, eval-gate, use. A model nobody measured is not a model we
   recommend — but a model the machine can run should not be hidden either.
2. **Strata's own app (0.2.0: new page, new TUI) should be reachable through Golem.** When
   the Strata engine is installed, the web console and the TUI get an "open Strata" entry
   that launches/links to it. Golem does **not** proxy or translate its traffic — that
   would be a gateway, which Golem never is. It just starts the app and points the
   browser/terminal at it, the same way it starts an engine.

## Why Strata is interesting (and what stops it today)

Strata's layout — experts in system RAM, a lookup table on SSD, only the hot layers and KV
on the GPU — runs models nothing else can touch on a 12 GB card:

- **Qwen3.8-Flash-Next "Coder" build**: the 125B model pruned to 256/512 experts at
  ~1.89 bpw. ~23 GB of experts in RAM + ~6 GB on the GPU; 91.3% of the full model's
  SWE-bench Verified (98.7% LiveCodeBench), ~50 t/s on a 12 GB card with 32 GB RAM
  (vramcalculator.com/strata-qwen3-8-flash-next/). 2-bit territory, so quality must be
  gated, not assumed.

What stopped adoption (measured 2026-10-19, `bench/results.jsonl` reject rows): v0.1.45
**cannot serve 200K context on 12 GB** — its VRAM cost is ~57 KB per context token vs
llama.cpp's ~12.7 KB, so 200K needs ~11.5 GB for KV alone — and where it does run it was
15% slower than llama.cpp. The 0.2.0 line brings faster defaults for 12–16 GB cards;
their own notes claim only ~+2% hybrid-mode decode on an RTX 3060.

## The prerequisite: a 30-second probe

Before any of the below matters, Strata has to clear viability on the box:

```
strata engine, preset ctx 236544 → either it starts and serves, or it reports 0 MiB free
```

- Starts → paired 32K bench vs llama.cpp, then the 200K eval gate. Both PASS → the engine
  becomes a real candidate and this page becomes a roadmap, not a memo.
- Refuses → nothing below ships; Golem keeps llama.cpp and this page waits for 0.2.x.

## Design

### Model discovery and the fit check

- `golem models --engine strata` lists what the installed Strata can serve (its setup
  writes the catalog the app reads; the adapter in `engines/strata/` exposes it).
- Fit = the same arithmetic the tuner already does, with Strata's layout: GPU side
  (non-expert weights + KV at its per-token cost) against the measured VRAM wall
  (~11.8 GB before the Windows sysmem cliff on the reference box), expert side against
  free system RAM, lookup table against free disk. The numbers come from the engine's
  own `--inspect`, not from vendor pages.
- Output: each model marked **fits / tight / no**, with the reason. "Tight" models are
  offered with a smaller context.

### The loop, identical to any other model

A Strata model that fits follows the standard path — no special casing:

1. preset section with `; Download:` line → `golem pull <preset>`
2. paired bench (best-of-3, same session, same size class)
3. `golem eval --long 200000` — the gate that decides everything
4. adopt as default, or keep as an *opt-in* preset users can request by name

Step 4 is the "allow users to use them if they want": router mode already serves any
preset section on request; the web console and `golem models` list them with their
measured numbers, and nothing becomes the default without the eval PASS.

### Exposing the 0.2.0 app

- `engines/strata/engine.ini` grows a `app = <command>` entry.
- Web console: an "Open Strata" button that runs it (or focuses it if already running)
  and shows the URL. TUI: a menu entry that spawns it in a new window.
- Golem never sits between the app and the engine; the app talks to its own engine the
  way Strata ships it. Start/stop still flows through Golem's pid-file control so the
  two UIs cannot fight over one engine.

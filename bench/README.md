# Benchmark records

`results.jsonl` holds one JSON object per line: every speed and quality measurement taken on
this repo, written by the tools themselves so a number never travels without what produced it.

| Command | Writes |
|---|---|
| `./golem bench` | decode best/median, MTP acceptance, prefill, VRAM, RAM |
| `./golem eval --long N` | tool calls, needle verdict, prefill and decode at depth |
| `./golem tune [preset]` | one row per sweep point (offload split, batch, threads, spec) |

The file is **append-only**: a correction is a new row with a `note`, never an edit. Rows taken
while something was downloading or building carry `"quiet": false` and the page greys them out.

## What a row contains

| Field | Meaning |
|---|---|
| `ts`, `kind`, `host` | UTC time; `bench` / `eval` / `reject`; anonymous hardware slug (`rtx-3060_12g_32g_i7-12700kf`) |
| `engine`, `engine_build` | `llama.cpp b11379-1537a0a8b`, `ik_llama.cpp main-b5483…`, `strata v0.1.45` |
| `model`, `model_file` | the preset (or alias) and the GGUF it served |
| `ctx` | context actually served |
| `params_b`, `params_active_b` | total and **active** parameters — a 35B-A3B is 35.5B of weights but 3.5B of work per token |
| `moe`, `experts`, `n_layer`, `ctx_train` | model shape, from the GGUF header (`scripts/gguf-info.sh`) |
| `flags` | the preset section's effective flags (`[*]` merged with the model's own) |
| `decode_best`, `decode_median`, `draft_accept` | tok/s; speculative acceptance 0-1 |
| `prefill` | tok/s reading a ~28.6K-token prompt |
| `vram_mib`, `ram_used_gib` | GPU memory at record time; working set of the server process tree |
| `ram_free_gib` | system RAM free |
| `eval.{tools,needle,needle_tokens,depth_decode}` | the quality gate's verdicts |
| `quiet`, `note` | measurement hygiene; free text (why a row exists, why something was rejected) |

## Reading the numbers

- **Only compare within a size class.** A 0.5B at 269 t/s says nothing about a 35B at 59. The
  report groups by active parameters (≤2B, 2–8B, 8–20B, 20B+) and offers "t/s per active B" as
  the one honest cross-size ratio.
- **Only compare within a session.** Desktop load moves decode by ±3 t/s; downloads and games by
  far more. The `quiet` flag is a warning, not a guarantee.
- **Same flags, same context.** `ctx` and `flags` are part of the comparison: 231K context costs
  VRAM that 32K does not.

## Views

| What | How |
|---|---|
| Server page, always current | `./golem web` → http://127.0.0.1:8787 (live from `results.jsonl`, controls, models, engine log) |
| Static page for sharing | `./golem report` → `bench/report.html`, one self-contained file (no server, no CDN) |
| Markdown for GitHub | `./golem report --md` → `bench/RESULTS.md` with the tables below, rendered by GitHub |

GitHub does not render HTML or JavaScript inside a `.md` file (it strips it), so the interactive
page cannot be embedded in a README: the Markdown summary is the GitHub-friendly view, and the
static HTML is for sharing a file or GitHub Pages.

## Model facts in every row

`ctx`, `params_b`, `params_active_b`, `moe`, `experts`, `n_layer` and `ctx_train` are read from
the model's GGUF header by [scripts/gguf-info.sh](../scripts/gguf-info.sh) — no model load,
cached under `data/cache/`. They are recorded automatically; rows from before 2026-10-20 were
filled in with the same tool.

# Plan: best engine + auto-tune + benchmark database

Executable by any agent model: each phase lists files, steps and a "done when" check. Work
top to bottom; commit after each step that passes its check.

## Status and next steps (update this block every session)

As of 2026-10-19 (`experiment/token-machine`):

- **Phase 1 done** (db1f63b): `./golem bench|eval` append to `bench/results.jsonl`, now with
  engine RAM (`ram_used_gib`) and a build fallback for engines whose `/props` omits it.
- **Phase 4 done** (708a933): `./golem report` / `golem.cmd report` (menu 6) →
  `bench/report.html`, verified rendering in headless Edge.
- **Phase 2 done for Windows** (c0cadff, e70a53b): `engines/<name>/engine.ini`, installs
  under `.local/engines/`, flag translation, `start-windows.ps1 -DryRun`.
- **Engine gate DONE** (31e475a): llama.cpp keeps the default. Measured, paired, same
  session: llama.cpp 58.1 t/s at 236544 ctx; strata 49.8 at 32768 and **cannot run 236544
  at all**; ik_llama.cpp 16.7 at 236544. Both alternatives are `reject` rows in
  `bench/results.jsonl` with the reasons; their adapters stay for a future build.
- **Phase 3 done, verified on a small model** (a89eb41 + 5bed6d7): `./golem tune` probes,
  seeds from `llama-fit-params`, sweeps, records every point, writes
  `data/tuned/<host-slug>/<preset>.ini` + `data/tuned/models.effective.ini`, and can gate
  the winner with the 200K eval. Verified end to end with `qwen2.5-0.5b-instruct`
  (candidates restarted, measured, selected, effective preset served). **Not yet run on the
  35B preset** — it needs ~15 quiet minutes.
- **Benchmark records** carry what the model is (`ctx`, `params_b`, `params_active_b`, `moe`,
  `experts`, `n_layer`, `ctx_train`, from `scripts/gguf-info.sh`, header-only + cached) so a
  number never travels without the thing it measured.
- **Console** (cd5a898): `./golem web` / `golem.cmd web` (menu 7) serves the results page
  plus model management, controls, engine log and an add-model form; it stays up with no
  engine running, which is the user's ask.

Next, in order:

1. 🔬 `./golem tune ornith-1.5-35b` on a quiet machine (no game, no download): expect the
   sweep to land near the hand-tuned `n-cpu-moe 36` (~58 t/s at 231K) and pass the gate.
   Then decide whether to point `MODELS_PRESET` at `data/tuned/models.effective.ini`
   permanently.
2. 🔬 First-run experience on a clean machine: clone → `golem.cmd` → Start should end with a
   tuned, running model. The pieces exist; nobody has walked the whole path on a fresh box.
3. Engine search loop (Phase 5) when a new engine or build appears — the adapters make it a
   paired bench plus the 200K eval.
4. Housekeeping: delete `data/models/Ornith-1.5-35B-A3B-APEX-MTP-Compact.gguf` (rejected,
   16 GB) and the two stale Qwen3.8 quants if the user agrees.

Goal: one command gives the fastest *correct* local setup on a single consumer GPU, picking
the engine, the model preset flags and the offload split automatically, on Windows (with or
without Git Bash), Linux and Docker. Every measurement is stored and viewable on a page.

## Ground rules (from the user, non-negotiable)

- Nothing is adopted without `./golem eval --long 200000` PASS (tool calls + 200K needle).
- Benchmarks: quiet desktop, best-of-3, **never while anything downloads or builds**, and
  compare only against a baseline measured in the same session (the day moves ±3 t/s).
- Backend-agnostic: engine, model and flags come from config, never hardcoded in scripts.
- Golem only serves models: no MCP, no gateway/translation proxy. Claude Code talks to the
  engine's native `/v1/messages`.
- Every feature works on: Windows native via `golem.cmd` (no Git Bash) and via `./golem`
  (Git Bash), Linux native, Docker. PowerShell 5.1 compatible on the Windows path.
- Commits: Conventional Commits — `feat(scope):`, `fix(scope):`, `docs:`, `perf:`,
  `refactor:`, `chore:`, `test:`. Body says what was measured. Branch: `experiment/token-machine`.
- Delegation for research: file reading → deepseek-v4.1-flash; web search → glm-5.3-flash with
  Z.ai's built-in web search only.

## Hardware reference (production box)

RTX 3060 12 GB (Ampere), 32 GB RAM, i7-12700KF (8P+4E, 16 threads used). Windows driver
silently falls back to system memory above ~11.8 GB VRAM (prefill −40%) — the tuner must
treat that as a hard wall, not a soft cost. Desktop VRAM use varies 0.5-1.6 GB.

Current best (2026-10-19, llama.cpp b11379 Windows native, Ornith-1.5-35B-A3B Q4_K_M):
decode 59.5 t/s, MTP acceptance 66%, prefill 832 t/s @28.6K, VRAM 11.0 GB, needle 211.8K
PASS, decode at depth 30.6 t/s. Config: `--n-cpu-moe 36 -b/-ub 2048 --spec-type draft-mtp
-ctk/-ctv q8_0 --load-mode mlock`, ctx 236544, 16 threads. This is the baseline to beat.

---

## Phase 1 — benchmark database (do first: everything else writes into it)

**Files:** `scripts/benchmark-ai.sh`, `scripts/eval-model.sh`, new `bench/results.jsonl`,
new `bench/README.md`.

1. Both scripts get `--record` (default on when called via `./golem bench|eval`, `--no-record`
   to skip). At the end they append **one JSON line** to `bench/results.jsonl`:

   ```json
   {"ts":"2026-10-19T19:50:00Z","kind":"bench","host":"rtx3060-12g_32g_12700kf",
    "os":"windows","mode":"windows","engine":"llama.cpp","engine_build":"b11379",
    "model":"ornith-1.5-35b","model_file":"Ornith-1.5-35B-Q4_K_M.gguf",
    "flags":{"n-cpu-moe":36,"ubatch-size":2048,"spec-type":"draft-mtp","ctx-size":236544},
    "decode_best":59.5,"decode_median":59.3,"draft_accept":0.66,"prefill":832,
    "prefill_tokens":28592,"vram_mib":11039,"ram_free_gib":18.0,
    "eval":{"tools":"PASS","needle_tokens":211818,"needle":"PASS","depth_decode":30.6},
    "quiet":true,"note":""}
   ```

   - `host` = a stable, anonymous slug from GPU name + VRAM + RAM + CPU model (no hostname,
     no IP — the repo is public).
   - `flags` = the effective flags of the served preset section (parse `config/models.ini`
     section `[MODEL_ALIAS]` merged with `[*]` and any tuned overlay, Phase 3).
   - `engine_build` from the engine adapter (Phase 2); until then read `LLAMA_WIN_BUILD` /
     the docker digest.
   - `quiet` = no curl/llama/hf download process running and CPU idle >80% before start;
     if false the line is still written but marked, and the page greys it out.
   - Write with plain `printf` (no jq/python needed), one line, append-only.
2. Seed the file with the 2026-10-19 gate rows (Qwen3.6, Ornith, Ornith APEX) from
   `docs/models.md`, `"note":"backfill"`. Older tables (Docker era, retired models) stay out.
3. `bench/results.jsonl` is committed (public data, small). Results are append-only; never
   edit old lines — a correction is a new line with `note`.

**Done when:** `./golem bench` and `./golem eval` each append a valid line
(`awk` check: every line starts with `{` and ends with `}`) on Git Bash and via `golem.cmd`.

## Phase 2 — engines as adapters

**Files:** new `engines/<name>/engine.ini` + `engines/<name>/start.{sh,ps1}`; launchers
(`scripts/start-local-ai.sh`, `scripts/start-windows.ps1`, `compose.yml`) call the adapter of
`ENGINE` in `.env` (default `llama.cpp`).

Installs live inside the repo, git-ignored, like llama.cpp today (`.local/bin-windows`,
`.local/bin`): `.local/engines/<name>/<build>/` with a `BUILD` marker written only after the
install verified (same rule as `start-windows.ps1`). Downloaded on first use of that engine by
the launcher; nothing outside the repo is referenced. Migrate llama.cpp into the same layout
in this phase (keep a fallback to the old paths for one release).

`engine.ini` describes one engine declaratively:

```ini
name = ik_llama.cpp
; pinned build + per-platform download/build recipe (verify checksum)
build = <tag-or-commit>
windows_cuda_url = ...
linux_build = cmake -B build -DGGML_CUDA=ON ...
docker_image = ...@sha256:...
; capabilities the selector filters on
api_anthropic = true          ; /v1/messages - REQUIRED for Claude Code
api_openai = true
router = false                ; --models-preset support
mtp = true                    ; draft-mtp / embedded nextn heads
moe_offload = n-cpu-moe       ; n-cpu-moe | override-tensor | none
gguf = true
; flag translation from Golem's canonical keys (the llama.cpp names) to this engine
flag.n-cpu-moe = --n-cpu-moe
flag.spec-type = <unsupported>
```

Rules:
- Canonical config stays in llama.cpp flag names (`config/models.ini`); adapters translate.
  A flag an engine lacks → the adapter drops it and logs a `[WARN]`, never fails silently.
- An engine without `/v1/messages` can only be used for OpenAI clients; the selector never
  picks it for `./golem claude`.
- An engine without router mode is run single-model with the default section's flags.

Candidate order (see "Engine research" below): llama.cpp mainline (current), ik_llama.cpp,
Strata, then ExLlamaV3/TabbyAPI only if it supports the qwen35moe arch.

**Done when:** `ENGINE=llama.cpp` through the adapter gives the same bench numbers (±3%,
same session) as today, and a second engine starts, serves `/v1/chat/completions`, and is
benched and recorded.

## Phase 3 — auto-tune (`./golem tune [preset]`)

**Files:** new `scripts/tune.sh` + `scripts/tune.ps1` (same algorithm, both shells), output
`data/tuned/<host>/<engine>/<preset>.ini` (git-ignored; per-machine).

Algorithm (analytic seed → short measured search, ~15-25 min budget):

0. **Fit seed** — measured 2026-10-19 (Ornith, paired): `--fit on --fit-target 1024 -ngl auto`
   (no n-cpu-moe) = 57.4 / 57.3 t/s, 818 prefill, 11.1 GB vs hand-tuned n-cpu-moe 36 =
   59.4 / 58.7, 807, 11.0 GB → fit is within ~3.5% with zero tuning. So: fit is the default
   for untuned hosts (zero-config first run), and the tuner seeds from `llama-fit-params`
   (note: it rejects `--spec-type`, so MTP overhead is not in its estimate) then searches.
1. **Probe:** total/free VRAM (`nvidia-smi` / `rocm-smi`), free RAM, physical cores (P vs E),
   OS, desktop VRAM in use. VRAM budget = min(total − 0.45 GB, 11.8/12 × total) − desktop.
2. **Model facts from the GGUF header** (read with a tiny header parser, no python):
   layers, experts, expert tensor bytes per layer, KV bytes/token at the chosen cache type,
   has embedded MTP (`*.nextn.*` tensors).
3. **Seed:** pick `n-cpu-moe` so that dense weights + (layers − n) × expert bytes + KV(ctx) +
   compute buffer (≈ ubatch-dependent, measure once) ≤ VRAM budget. Pick ctx = min(native,
   requested `CONTEXT_SIZE`, what fits). RAM check: mlock'd CPU experts + 4 GB headroom ≤ free
   RAM, else fall back to `load-mode auto` or a smaller quant and say so.
4. **Search** (each point = server restart + warm request + 3-run decode + 1 prefill):
   - `n-cpu-moe` ∈ {seed−2 … seed+2}, keep only points under the VRAM wall (detect sysmem
     fallback: prefill drops >25% vs neighbour → reject point);
   - `ubatch` ∈ {1024, 2048} at the best n; threads ∈ {P-cores, all-cores};
   - spec on/off when the model has MTP; `spec-draft-n-max` 2 vs 3 only if acceptance >60%.
   - Objective: maximize decode_median, tie-break prefill; reject any point with VRAM > wall.
5. Run `eval --long 200000` on the winner; only a PASS is written to the tuned overlay.
6. Every point goes to `bench/results.jsonl` with `"kind":"tune"`.
7. Launchers merge `data/tuned/...ini` over `config/models.ini` into a generated
   `data/tuned/models.effective.ini` and serve that (llama-server takes one preset file).

**Engine selection** (built): plain `./golem tune` sweeps *every* installed engine whose
`engine.ini` says `tune = true` (llama.cpp, ik_llama.cpp) and picks engine + split together;
`--engine NAME` is the quick single-engine path. Engines that cannot be swept are skipped with
their reason from `engine.ini` (`tune_reason`), e.g. Strata, which serves its own catalog model
and ignores preset flags. The winner is gated with the 200K eval; if it fails, the next-ranked
candidate is tried, so a fast-but-wrong engine cannot win. `--adopt` writes `ENGINE=` and
`MODELS_PRESET=` into `.env` (backup `.env.bak`).

**Done when:** on the production box, `./golem tune ornith-1.5-35b` from a fresh overlay lands
within 3% of the hand-tuned 59.5 t/s and passes eval, unattended. Verified on a 0.5B model
(both engines swept, ranked, overlay + winner.txt + effective preset written); the 35B run
still needs a quiet machine.

## Phase 4 — results page

**Files:** new `bench/report.html` template + `./golem report` (bash) / `golem.cmd report`
(PowerShell).

- `report` inlines `bench/results.jsonl` into a **single self-contained HTML file**
  (`bench/report.html`, data in a `<script type="application/json">` block) — opens from
  `file://`, no server, no python; also publishable as-is on GitHub Pages.
- Views: leaderboard (best eval-PASS config per model × engine × host), decode vs prefill
  scatter, history per model over engine builds (catches long-context regressions),
  tune sweep chart (n-cpu-moe vs decode with the VRAM wall marked). Filters: host, engine,
  model, kind, quiet-only (default on). Light/dark.
- No external CDNs except one pinned chart lib (or hand-rolled SVG); works offline.

**Done when:** `./golem report` produces a page that shows today's Ornith vs Qwen3.6 rows
correctly in a browser.

## Phase 5 — engine search loop (repeatable, monthly)

1. Research (glm-5.3-flash, Z.ai web search): new engines/forks/builds since the last run;
   screen with the capability filter (Windows or Linux native, GGUF or easy conversion,
   MoE CPU offload, OpenAI API; Anthropic API for the Claude Code path).
2. Add `engines/<name>/engine.ini` for survivors, pinned build.
3. `./golem tune --all-engines` on the default coder, quiet desktop, paired baseline.
4. Eval gate → adopt or record why rejected (a `"kind":"reject"` line with `note`).
5. Same loop for new models (it is what we did for Ornith: presets → bench → eval → adopt).

## Engine research (2026-10-19, glm-5.3-flash web search; "unverified" = re-check first)

**Biggest finding — tune llama.cpp's own auto-fit first.** Mainline has `--fit on|off`
(default on), `--fit-target MiB` (default 1024 free per GPU), `--fit-ctx N` and a dry-run
binary `llama-fit-params` that prints the `-ngl`/`-ot` placement it would use. Fit only
touches unset args: our explicit `--n-cpu-moe 36` **disables it**. Maintainers say fit's
`-ot` placement handles MoE better than `--n-cpu-moe` (issue #27501). → Phase 3 step 0:
bench `fit` (no n-cpu-moe, `--fit-target` 600-1000) vs the hand-tuned n=36, paired. If fit
wins or ties, the tuner seeds from `llama-fit-params` instead of its own VRAM math.
Also watch llama.cpp RFC #24528 "MoE expert cache" (~60% gain reported) — re-bench when merged.

| Engine | Windows | Fits our model? | `/v1/messages` | Router | MTP | Notes |
|---|---|---|---|---|---|---|
| llama.cpp mainline | official CUDA zips | yes | yes | yes | yes | current; baseline |
| ik_llama.cpp | no official zip; Thireus fork releases (unverified targets); Docker cu12 | qwen35moe yes, fused DeltaNet; **Unsloth UD/_XL GGUFs may crash** (official Q4_K_M ok) | no (unverified) | no | yes (Qwen3.5/3.6 PRs) | best CPU-expert prefill (2× reported); own auto-fit (`--fit` margin 1872 MB); IQK quants |
| Strata 0.2.0-beta (engine 0.1.45.3) | **standalone tarballs** (`strata-engine-0.1.45.3-windows-x86_64-cuda-cuda13.tar.gz` 553 MB + `strata-control-…zip` 69 MB), so it installs headlessly under `.local/engines/strata/` — the .exe files are the app, not the only option | Qwen3.6-35B-A3B "Coder" size for 32 GB | yes | no (unverified) | yes | own C++ ggml-derived engine, VRAM expert cache; 0.2.0 claims +10.2% decode on an RTX 3060 12 GB (60.36 → 66.52 t/s, 8/8 pairs) from per-architecture defaults; `strata bench run`; port 8080. **The engine is still 0.1.45.x, so the 200K blocker stands** — see the rejected row in `bench/results.jsonl` |
| ExLlamaV3 / TabbyAPI | Python | needs EXL3 conversion; qwen35moe/DeltaNet support unverified | no | n/a | ? | MoE CPU offload since 1.4.9; only if arch supported |
| croco.cpp (koboldcpp fork) | CI artifacts | IQK quants | no | no | ? | hobby fork, low priority |
| vLLM, SGLang, KTransformers, MLC, mistral.rs, Lemonade, Ollama, LM Studio, llamafile | mostly no / wrappers | — | mostly no | — | — | skip: no single-GPU expert offload, Linux-only, or wrap llama.cpp |

Claude Code path needs `/v1/messages` → today only llama.cpp and Strata qualify; ik_llama.cpp
serves OpenAI clients only (Golem has no translation proxy by design).

Ideas worth borrowing from Strata's design (public repo, not its code): a memory planner that
refuses configs that don't fit instead of overcommitting; a calibrator that keeps a knob only
if it beats the default by ≥3% in **interleaved** A/B/A/B runs (adopt this in the tuner); bench
records that carry the engine binary sha256 (add `engine_sha256` to our record).

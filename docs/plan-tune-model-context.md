# Plan: tune the setup, not just the flags (model choice + max context + speed floor)

Status: **design agreed 2026-10-20 — floors confirmed by the user: decode 25 t/s,
prefill 350 t/s, context ceiling = the model's trained ctx (extrapolation only when the
user pins `--ctx` higher).** Extends
`./golem tune` (docs/plan-engines-autotune.md, phases 1-4 done). Nothing here changes
today's behavior until the floors are confirmed.

## The idea

Today `tune <preset>` answers "what flags run *this* model fastest here?". It should
answer the question the user actually asks on a new machine:

> Give me the smartest model I can run, at the biggest context it can truly serve,
> without falling below usable speed.

Three changes, in order of size:

1. **Context maximization** — the sweep grows `ctx-size` until the fit breaks or the
   gate fails, instead of inheriting whatever the preset says. A 0.5B that can truly
   serve 500K should not sit at 8K because a smoke-test preset said so.
2. **Speed floor** — candidates below the floor are excluded from the ranking (they
   still get recorded, marked as below-floor). The floor is what "appropriate for
   consumer hardware" means concretely; the user can move it.
3. **Model selection** — `tune --auto` runs the same loop over every installed preset
   (and, later, everything the model registry says fits — see strata-models.md), ranks
   by size-class smartness, and proposes the best setup that clears floor + gate.
   `tune <preset>` keeps today's meaning.

## The floor (proposed, pending agreement)

Grounded in measurements on the reference box (RTX 3060 12 GB / 32 GB RAM —
docs/models.md, bench/results.jsonl):

| Floor | Value | Why this number |
|---|---|---|
| decode (median, at chosen ctx) | **25 t/s** | ~30 t/s at 211K depth felt fine in real sessions; 23 t/s was "usable but sluggish"; 16.7 t/s (ik_llama.cpp) was rejected as unusable. 25 sits between fine and sluggish. |
| prefill | **350 t/s** | 820 t/s ≈ 35 s for Claude Code's 27K first prompt; the old 340 t/s config ≈ 80 s — tolerated, but not as a *tuning result*. 350 admits honest slow configs, refuses the 80 s outcome. |
| decode at full context | **15 t/s** | a giant context you can't use at depth is a fake win; 30 t/s measured at 211K on the default. |

The spill check (prefill collapse vs the engine's own best) stays as-is — it is
relative and catches the sysmem cliff; the floor is absolute and catches
slow-but-not-spilling.

## User control

Knobs, in .env with per-run overrides (GOLEM_* so .env re-sourcing can't eat them):

| Knob | Default | Meaning |
|---|---|---|
| `TUNE_MIN_DECODE` | 25 | tokens/s median decode floor |
| `TUNE_MIN_PREFILL` | 350 | tokens/s prefill floor |
| `TUNE_MIN_DEPTH_DECODE` | 15 | tokens/s at full context |
| `TUNE_CTX_POLICY` | max | `max` = grow until fit/gate stops it; a number = pin that context |

CLI mirrors: `./golem tune --ctx N` (pin), `--min-decode/--min-prefill N` (move the
floor for this run), `--auto` (model selection). The web console and TUI expose the
same three numbers as fields, not hidden flags — the user should be able to decide how
much context they want and how slow is too slow.

## How maximization runs

Per candidate split (after today's n-cpu-moe sweep picks the split):

1. seed ctx from `llama-fit-params` at that split (it computes the KV budget exactly);
2. step ctx up (×2 from the preset's value, then binary search) while the fit holds
   and VRAM stays under the measured wall;
3. cap at the model's `ctx_train` unless the user explicitly asks for more
   (extrapolation worked in testing — the 0.5B passed a 211K needle on a 32K-train
   model — but it is per-model luck, so it must be asked for, not default);
4. gate at 90% of the final ctx (needle + tools, as today). Gate fails → step ctx
   down one notch and re-gate; a model whose gate only passes at a context below the
   user's floor… is simply reported with the context it can truly serve.

Every step is a recorded row (kind `tune`), so the ctx-vs-speed frontier a machine can
reach is in the database, not in a log.

## Order of work

1. ✅ floors + `TUNE_*` knobs + floor enforcement in the ranking, plus a topology-derived
   thread sweep in the refine stage (hybrid P/E CPUs probe {P, P+HT, all} — measured
   16-of-20 best on the reference box, docs/models.md "Threads") — all tune.sh only;
2. ✅ ctx maximization loop (doubling + one midpoint probe) gated at 90% of the final ctx;
3. ✅ `--auto` model selection (`scripts/tune-auto.sh`): cheap sweep per installed preset,
   rank by `; Quality: N` then parameters then context then speed, then a full gated tune
   of the best, falling back down the ranking. Verified on the 0.5B preset with two
   sections at different quality scores plus one undownloaded section (skipped);
4. registry-driven candidates (strata-models.md) once that probe clears.

Still open from this plan: the web console has a `tune` endpoint but no control that
sends `--auto` (the validation would accept it), and `start-local-ai.sh` (Linux/docker)
never got the Windows launcher's `; Download:` auto-fetch, so `--auto` there is untested.

# Tuning

`golem tune [preset]` answers the question a new machine asks: what is the fastest setup
this card can serve *this* model with, at the largest context it can genuinely hold,
without falling below a speed that is still usable? It measures, decides, gates the
winner with the 200K eval, and only then writes anything that serves.

```bash
golem tune ornith-1.5-35b            # search, gate, write the overlay
golem tune ornith-1.5-35b --adopt    # ...and point golem.toml at the result
golem tune --auto --adopt            # pick the best installed model first
golem tune <preset> --dry-run        # the plan, and what it would spend
```

Every point is a full model load and a restart - for the default coder a 20 GB read on a
32 GB box - so the tuner's design is about spending as few of them as possible. Run it on a
free desktop: the numbers mean nothing next to a game or a download, and the box needs its
RAM.

## The floors

A candidate under any floor is excluded from the ranking, however good it looks next to a
worse one, and the run says which and why.

| Floor | Default | Why this number |
|---|---|---|
| decode (median) | 25 t/s | ~30 t/s felt fine in real sessions, 23 was sluggish, 16.7 (ik_llama.cpp) was rejected as unusable |
| prefill | 350 t/s | 820 t/s reads Claude Code's ~27K first prompt in ~35 s; 340 t/s took ~80 s - tolerated once, not a tuning result |
| decode at depth | 15 t/s | a giant context that is unusable at depth is a fake win; the default coder does ~35 at 200K |

`TUNE_MIN_DECODE`, `TUNE_MIN_PREFILL` and `TUNE_MIN_DEPTH_DECODE` in `golem.toml` move
them; `--min-decode`, `--min-prefill` and `--min-depth-decode` move them for one run. The
context ceiling is the model's trained context: past it, extrapolation works per model
rather than in general (a 0.5B passed a 211K needle on a 32K-trained model), so it has to
be asked for with `--ctx N`.

## The wall

Every point's VRAM is the bench's reading of the whole card, desktop included. A setup
fits when that reading is at most **the card's size less `TUNE_VRAM_HEADROOM_MIB`**
(1024 by default, `--vram-headroom` per run). The headroom is the desktop's room to grow:
its own use drifts between half a gigabyte and one and a half over a day, and past the
card's size the Windows driver spills into system memory silently. A spill also shows as a
prefill collapse against the best the same engine reached at the same `ubatch-size`, which
the ranking excludes (`golem_core::tune::exclusion`) - so the reading and the measured speed both have to agree
that a point fits.

The engine is stopped before the card is probed (a loaded model would read as "desktop"),
and put back after the run on the configuration as it then stands.

## The search

1. **Seed.** `llama-fit-params` places the model for the target context without loading
   it; the number of layers it keeps on the CPU is the seed split. Without the tool, the
   preset's own `n-cpu-moe`, else arithmetic from the model's size and the card.
2. **The split frontier, by bisection.** A lower `n-cpu-moe` puts more experts on the
   card, so the splits that fit are a contiguous range: measure the seed, step up by two
   while it spills, then halve the gap between what fits and what spilled. Three to five
   loads locate the frontier to one layer where a walk took nine. A split that will not
   load is an upper bound, not a lost point. (`--candidates a,b,c` measures exactly those.)
3. **Refine - one probe per knob that can still matter.** `ubatch-size 1024` (prefill's
   lever; the collapse test catches a bad one). A thread count only when decode is within
   25% of the floor - the knob moves decode by a few percent and costs a whole load. MTP
   off, only when the model has a head.
4. **Rank** what is not excluded by decode, and take the best. Decodes within 3% of the
   best (`DECODE_TIE`) are a tie, and the tie goes to the faster prefill: a gap that small
   is run-to-run noise, and prefill is paid on every first prompt.
5. **Grow the context, by prediction.** VRAM is a straight line in the context at a fixed
   split. The winner's own point is the line's first point; one doubling (capped at the
   trained context) gives the second; from there the largest context under the wall is
   arithmetic, and one load confirms it. Two to three loads where the doubling ladder took
   four or five. `TUNE_CTX_POLICY = "ladder"` or `--ctx-ladder` keeps the old ladder for a
   paired comparison; `--ctx N` pins a context.
6. **Gate once**, in the serving configuration: the preset's own load mode (sweep points
   load with mmap so a 32 GB box survives the restarts; the gate keeps `mlock`), a needle
   at 90% of the tuned context capped at `GATE_TOKENS`, tool calls, and decode at depth
   against its floor. A failed gate moves to the next-ranked candidate.

Budget for the default coder: about ten loads - three to five for the frontier, one or two
refine probes, two or three for the context, and the gate - against the seventeen the
original grid sweep spent.

### What a failed context probe means

The prediction has one rule that the obvious wiring gets wrong, and it is worth stating
because it is easy to undo by accident:

- **Over the wall** is a real measurement - the model loaded and the card went past the
  wall - so the failed point is the line's new top, and the next prediction comes from it.
  It lands near the true ceiling.
- **Under the decode floor** is not a point at all: the model spilled, and its VRAM
  reading describes where the spill ended up rather than what the context costs. That
  failure bisects between the last context that fit and the attempt.

Falling back to the ladder's midpoint after a failed *prediction* is the trap: from a
4096 calibration and a 240000 attempt it answers 122048, where the ladder itself would
have reached 249344. `golem_core::tune::ContextSearch` holds the points and the rule.

## What it writes

Nothing that serves until the gate passes. The gate runs from a staged candidate
(`data/tuned/candidates/<preset>-gate.ini`); on a pass the winner's overlay - only the
keys it changed, plus the sweep as comments - goes to
`data/tuned/<host>/<engine>/<preset>.ini`, and `data/tuned/models.effective.ini` is
rebuilt as the base preset (`config/models.ini`, or `MODELS_PRESET` when it names another
file) with every gated overlay for this machine merged in. A tune never starts from the
effective file, so one tune's answers never become the next one's assumptions.

`--adopt` then backs up `golem.toml` and sets `ENGINE` and `MODELS_PRESET` to the
effective preset. `data/tuned/<host>/winner.txt` records the verdict and every point, and
every point is also a row in `bench/results.jsonl`.

## `--auto`

Ranks every preset whose GGUF is already on disk by its `; Quality: N` comment (a vendor
score - a ranking input, never a reason to adopt) or by size, gives each a cheap search,
then tunes and gates the best, falling back down the ranking. A model that is not
downloaded is skipped, never fetched mid-measurement, and it refuses to start while a
download, a build or a game is loading the machine.

## Measured

`golem tune ornith-1.5-35b --adopt`, 2026-10-24, on a desktop that was not quiet (1.37 GB
of VRAM and ~16 GB of RAM in use by other applications):

| Step | Loads | Points |
|---|---|---|
| split frontier | 4 | n31 spills (12070 MiB), n33 spills (11823), **n35 fits** (10926), n34 over the wall (11396) |
| refine | 2 | `ub1024` 62.5 t/s decode, 555 prefill, 9790 MiB; MTP off 50.7 t/s |
| context | 1 | 262144 (the trained context) at 10254 MiB, 62.3 t/s |
| gate | 1 | 200K needle **PASS**, 519 t/s prefill and 36.3 t/s decode at depth |

Eight loads to a gated verdict, against seventeen for the original sweep; the frontier is
the sweep's own n-cpu-moe 35. The verdict differs from the 2026-10-20 tune in two ways: the
context reached the full 262144 (that tune stopped at 249344), and `ubatch-size 1024` won
on decode (62.5 against 55.1 at 2048), which costs prefill - 505 t/s on an 8K prompt
against the 878 that `ub2048` measured on a quiet desktop in October. Both `ub2048`
points of this run were measured on the busy desktop.

The quiet-desktop pair, 2026-10-25: n-cpu-moe 35, ctx 262144, same session, best of
three benches of three runs each.

| Setup | Decode | Prefill 30K | Prefill 8K | VRAM (whole card) |
|---|---|---|---|---|
| `ub1024` | 63.9 t/s | 559 t/s | 564 t/s | 10358-10472 MiB |
| `ub2048` | 63.6 t/s | 866 t/s | 889 t/s | 11635-11807 MiB |

Decode is a tie and `ub2048` prefills 55-58% faster, but at the trained context it reads
400-550 MiB over the wall, so the tuner was right to refuse it there. Two rules changed
with it: the spill check compares within one `ubatch-size` (against `ub2048`'s 889,
`ub1024`'s 564 read as a spill), and a decode tie goes to prefill. Whether `ub2048` at
the largest context under the wall beats `ub1024` at the trained one is the next tune's
question.

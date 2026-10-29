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
32 GB box - so the tuner's design is about spending as few of them as possible, and as
little bench inside each as the decision needs. Run it on a free desktop: the numbers mean
nothing next to a game or a download, and the box needs its RAM.

A point is benched with the wall in hand (`bench --wall`): the warmup loads the model, and
from then on the card holds its weights, KV cache and compute buffers, so a reading over the
wall after the warmup is over the wall at the end. Such a point stops there - its VRAM is
the measurement, its speeds are never taken - and is excluded like any point over the wall;
it records no row in `bench/results.jsonl`, and `winner.txt` lists it as "over the wall
once loaded". The splits past the frontier and the context probes that overshoot are most
of these.

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
   25% of the floor - the knob moves decode by a few percent and costs a whole load. When
   the file carries an MTP head, the other side of the preset's `spec-type`: MTP off for a
   preset that drafts, MTP on for one that does not (`golem pull` switches it on for such
   a file, and the probe keeps it honest).
4. **Rank** what is not excluded by decode, and take the best. A point over the wall is
   excluded however fast it ran: the splits the bisection measured past the frontier
   decode faster precisely because they hold more than the card can spare. A decode
   close enough to the best is a tie, and the tie goes to the faster prefill: prefill is
   paid on every first prompt. How close is the two benches' own spread (best run over
   median), held between 3% and 8% (`tune::tie_band`): a drafting model's decode moves
   with what each run accepts, and a busy desktop moves it further, so a gap inside the
   benches' own spread is noise, not a ranking.
5. **Grow the context, by prediction.** VRAM is a straight line in the context at a fixed
   split. The winner's own point is the line's first point; one doubling (capped at the
   trained context) gives the second; from there the largest context under the wall is
   arithmetic, rounded down to 1024 tokens so a re-fit does not chase a reading's few MiB
   of noise, and one load confirms it. A prediction that promises less context than 128
   MiB of VRAM buys at the fitted slope is not probed: on a busy desktop two readings of
   one setup land that far apart, and such a probe measures the desktop. Two to three loads where the doubling ladder took
   four or five. The slope is what the KV cache costs per token, the same for every split
   of one model at one `ubatch-size` and drafting, so a later candidate with the same three
   starts from the slope an earlier one measured and predicts from its own point alone: the
   doubling is spent once per run, not once per candidate. `TUNE_CTX_POLICY = "ladder"` or
   `--ctx-ladder` keeps the old ladder for a paired comparison; `--ctx N` pins a context.
   A candidate whose grown context cannot hold the gate - `GATE_TOKENS` at 90%, 223232
   tokens for 200000, or the trained context when that is smaller - is passed over for the
   next-ranked one, however fast it was: on a 12 GB card Ornith-1.5-9B with MTP stops at
   188416 tokens (its draft keeps a cache of its own), and without MTP it holds the 200K
   gate. Only when no candidate holds it - a model this machine cannot serve at that
   size - are the ones that fell short gated, largest context first, and the run says
   so. A pinned `--ctx` is the caller's choice and is not held to this.
6. **Gate once**, in the serving configuration: the preset's own load mode (sweep points
   load with mmap so a 32 GB box survives the restarts; the gate keeps `mlock`), a needle
   at 90% of the tuned context capped at `GATE_TOKENS`, tool calls, and decode at depth
   against its floor. A failed gate moves to the next-ranked candidate - except a gate
   whose model did not load at all: the next candidate loads the same way, so the run
   stops there and names the load mode as the suspect.

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
effective preset, and an engine that was up comes back on it. The console's and the
screen's Tune is `tune <model> --adopt`: it always gates, so a tune started there serves
its verdict once the gate passes. `data/tuned/<host>/winner.txt` records the verdict and every point, and
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
`ub1024`'s 564 read as a spill), and a decode tie goes to prefill.

`golem tune ornith-1.5-35b` under those rules, 2026-10-25, quiet desktop, from the console:

| Step | Loads | Points |
|---|---|---|
| split frontier | 4 | n31 spills (11965 MiB), n33 spills (11903), **n35 fits** (11037), n34 over the wall (11533) |
| refine | 2 | `ub1024` 63.8 t/s decode, 534 prefill, 9902 MiB; MTP off 50.8 t/s |
| context | 7 | 262144 over the wall (11606), then 246757, 246623, 246238, 246367, 246286, **246256** (11260) |
| gate | 1 | 200K needle **PASS**, 672 t/s prefill and 36.2 t/s decode at depth |

`ub1024` and `ub2048` tie on decode (63.8 against 63.0) and `ub2048` wins on prefill (839
against 534), so the verdict is `ub2048` at 246256 tokens - 94% of the trained context for
57% more prefill than `ub1024` at all of it. The context search spent five of its loads on
the last 500 tokens: readings of one context differ by up to 9 MiB, so each re-fit through
them predicted a few hundred tokens more. With predictions rounded down to 1024 tokens
(`CONTEXT_STEP`), the same readings stop the search after two loads, at 245760.

The same tune with `CONTEXT_STEP` in and the wall applied to the ranking, 2026-10-25 late,
quiet desktop:

| Step | Loads | Points |
|---|---|---|
| split frontier | 4 | n31 (11919 MiB), n33 (11845), n34 (11477) over the wall; **n35 fits** (10985) |
| refine | 2 | `ub1024` 64.1 t/s decode, 561 prefill; MTP off 51.1 t/s |
| context | 2 | 262144 over the wall (11543), **248832** (11249) |
| gate | 1 | 200K needle **PASS**, 671 t/s prefill and 35.9 t/s decode at depth |

Nine loads, and the verdict is the earlier one's split and `ubatch-size` at 248832 tokens:
63.5 t/s decode, 846 prefill. The wall had to move into the ranking first. On b11879 the
splits the bisection had measured past the frontier stayed rankable, and n34 - 60.3 t/s
at 11473 MiB - beat the frontier's 56.6 on decode and went to the gate over the wall.

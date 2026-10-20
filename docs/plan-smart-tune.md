# Plan: the binary does everything, and the tuner stops guessing by brute force

Status: **decided 2026-10-20; steps 1-3 are built**. The split is found by
bisection (`golem_core::tune::between`), the context ceiling is predicted from
two measured points and confirmed by one load (`Fit`, `next_predicted`), and
the refine stage spends one probe per knob with the thread probe only when the
decode is near the floor. What is left is step 4, which is a GPU session: one
full `tune` end to end on a free desktop, against the sweep's own load count
and verdict. The prediction's fallback is in place for it - `--ctx-ladder`, or
`TUNE_CTX_POLICY = "ladder"`, runs the old doubling ladder.

## 1. The binary is the app

Every script is a liability: it needs Git Bash on Windows, or WSL, or a second
PowerShell implementation of the same feature, and each one is a place where the
configuration and the binary can disagree about what is being served.

The end state is one executable plus `golem.toml`:

| today | becomes |
|---|---|
| `scripts/start-windows.ps1`, `start-local-ai.sh`, `stop-*.ps1/.sh` | `up` / `down` / `restart` in the binary - [plan-launchers.md](plan-launchers.md) |
| `golem.cmd`, `./golem` | a one-line shim while they are still useful, then nothing |
| `scripts/install-llama-cpp.sh` | `install.rs` (ported) |
| `scripts/setup-ai.sh` | `setup.rs` (ported) |
| `scripts/download-model.sh` | `pull.rs` (ported) |
| `scripts/gguf-info.sh` | `golem_core::gguf` (ported) |
| `scripts/health-check-ai.sh` | `golem status` (ported) |
| `scripts/tui.sh`, `web.ps1`, `report.ps1` | `tui.rs`, `web.rs`, `report` (ported) |
| `scripts/golem-bash.cmd` | nothing - it exists only to reach the fallbacks |
| `.env` | `golem.toml`, generated with its comments on first run (done) |

What that leaves the shell: nothing. A release is `golem.exe` + `golem.toml`; a clone is the
same, with the binary built or downloaded.

**The native UI** is a real direction, not a joke - the web page is the console because a
browser was the cheapest way to draw it, and it stays for now. The shape it would take is a
second front end over the same `api/state` snapshot and the same job runner: `tui.rs` already
proves that the console's data is a plain struct and the actions are a callback. Nothing in
this plan blocks it, and nothing here starts it.

## 2. The tuner finds the frontier instead of sweeping a grid

Today `tune` measures nine splits (`seed-2 … seed+6`), four refine points, and a context
ladder - **about seventeen model loads per engine**, each one a full 20 GB read and a
restart. The user's objection is the right one: most of those loads are spent confirming
what arithmetic already knows.

### What is known before any load

- `llama-fit-params` gives a seed split for the target context, free, no model load.
- The GGUF header gives the layer count and the model's size on disk.
- The card gives a VRAM wall (already measured: `vram_used` when idle is the desktop).
- **After one load**, `/props` and the bench's own VRAM reading give the model's real
  footprint at that (split, context) - which is the calibration everything else needs.

### The shape that replaces the grid

1. **Calibrate once.** Measure the seed. One point gives decode, prefill, and VRAM at a
   known (split, context).
2. **Find the cliff by bisection, not by enumeration.** VRAM is monotonic in the split: a
   lower `n-cpu-moe` puts more experts on the card, so the accepted set is a contiguous
   range from the seed downward. The frontier is therefore one binary search - measure
   halfway to zero, keep the fitting half, repeat - and **three or four loads** locate it to
   one layer. A split that fails to load is information, not a wasted point: it is the upper
   bound of the search, and the tuner already handles it.
3. **Predict the context ceiling, don't climb to it.** Two points at the same split and
   different contexts give VRAM per context token exactly. From that, the largest context
   that fits under the wall is arithmetic. The ladder then *confirms* the prediction with one
   point, instead of doubling four times to discover it.

   **What that confirmation must not do, on the way to being written.** The obvious wiring is
   predict, measure, and on a failure fall back to the ladder's existing rule - one midpoint
   between the last context that fit and the attempt. That rule is right for a ladder, where
   `current` is a rung one doubling below the attempt, and wrong for a prediction, where
   `current` is the calibration point near the *bottom*: seed 4096, prediction 240000,
   failure, midpoint 122048. The ladder would have reached 249344. A tuner that finds a
   worse context in fewer loads is not a win.

   So the fallback has to re-predict rather than bisect once, and the failed point is what it
   re-predicts from:

   - **A failure over the VRAM wall** is a real third point - the model loaded and the card
     went past the wall - so the two highest points, the failed one included, give a steeper
     slope and a second prediction that lands near the true ceiling.
   - **A failure by the decode floor** is not a point at all: the model spilled to system
     memory, and its VRAM reading describes where the spill ended up rather than what the
     context costs. There the honest answer is a bisection between the last context that fit
     and the failed attempt.

   Which of the two a failure is, is already decided - `maximize_ctx` computes `over_wall`
   and `under_floor` separately today. What is new is that the two now lead somewhere
   different.

   This is worth settling before the code, and it is also why step 4 is not optional: the
   margin between "predicts the ceiling" and "predicts one the card refuses" is a property of
   this machine's card, and only a measured run says which way it lands.
4. **Refine only the knobs that can still matter.** Threads: one point, and only when the
   observed decode is within reach of the floor. Batch/ubatch: `ub` is the prefill lever and
   the collapse test already catches a bad one - one alternative, not three. Spec decoding:
   one off-point, only when the model has an MTP head.
5. **Gate once**, on the winner, in the serving mode.

**Budget: five loads for the frontier, one calibration included, then one `ub`, one thread,
one spec and the gate - about ten**, against seventeen. Two of those ten are the context
ladder, which the prediction in step 2 below would replace with one confirmation.

### What this is not

Not "arithmetic instead of measurement". The plan's oldest rule is that the cliff is found
by measurement, never by arithmetic - a split that *should* fit still spills when the desktop
has taken an extra gigabyte. The arithmetic here does what arithmetic is good at (locating
the neighbourhood, sizing the context) and spends loads on the two things only a measurement
answers: does it fit *now*, and how fast is it.

### What the first real run showed (2026-10-20)

On `ornith-1.5-35b` the search found the frontier at **n-cpu-moe 35** - the value the old
17-load sweep also chose - in **five** loads: the seed (31) spilled, 33 spilled, 35 fit,
17 spilled (the old code's discarded upper bound, since fixed), and 34 was never reached
because the machine failed first. The rows are in `bench/results.jsonl.bak`'s successor
files; `n33` prefill 475 against `n35`'s 819 is the collapse the sweep exists to see.

The gate then failed to run at all: every restart from n34 onward exited `-1073741502`
(`STATUS_IN_PAGE_ERROR`) - Windows could not page in `cmd.exe` itself. That is this box,
not this algorithm: a 20 GB model on 32 GB leaves ~400 MB of free physical memory, and a
restart-per-candidate loop runs the system out of pages. The tuner is therefore something to
run with the desktop free and somebody watching; **per-preset benches through the router**
switch models in process and are the cheap way to refresh the database.

### Order

1. Bisection for the split (`golem_core::tune::between`, pure, tested). **Done.**
2. The context prediction from two points (pure, tested). **Done**:
   `golem_core::tune::{Fit, Probe, next_predicted}`, wired into the tuner's
   `maximize_ctx` with `--ctx-ladder` keeping the old rule reachable. Two points
   at one split fix VRAM per context token, and the ceiling is arithmetic from
   the wall; the first grow is still the ladder's doubling, because one point is
   not a slope. An over-wall failure re-predicts from the failed point - which is
   a real measurement - and only a floor failure bisects, because a spilled
   model's VRAM reading describes where the spill ended up. A test states the
   trap as a number: from seed 4096 and a 240000 attempt the midpoint rule
   answers 122048 where the prediction lands within 1% of the true ceiling.
3. Trim refine to the knobs above. **Done**: the stage was already one probe per
   knob (batch, threads, spec), and the one thing it still spent a load on
   unconditionally was the thread count - which the plan asks for only when the
   observed decode is within reach of the floor, because the knob moves decode by
   a few percent and the probe is a whole load. `thread_probe_worth_it` is that
   rule, with the margin named.
4. Re-bench, and compare the load count and the verdict against this sweep's.

Until step 4 passes, the old sweep stays reachable behind a flag (`--sweep all`), because a
tuner that finds a *different* winner in fewer loads is only a win if the winner is at least
as good.

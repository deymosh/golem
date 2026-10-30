# Roadmap

What is left, in order. The design is in [architecture.md](architecture.md) and
[tuning.md](tuning.md); this page is only the work.

Golem's job is to make the best local setup for a machine easy to find, prove and use: it
knows the engines, fetches models from Hugging Face, measures them on the machine at hand,
tunes them to the card and the RAM, gates a setup before it becomes a default, and hands
any agent the endpoint. "Best" means best at the work an agent does on that machine - a
tokens-per-second figure is one input, not the answer.

## Where it stands

Checked against the code and `results/results.jsonl` (230 rows on the reference box):

- **What holds up.** One binary owns the engine (`service.rs`); every decision is a pure,
  tested function in `golem-core` (599 tests); a row records host, build, flags and
  model file, so a number is always traceable; nothing is adopted without the 200K gate.
  The tuner places a model in eight to ten loads where a grid spent seventeen.
- **The gate measures plumbing, not work.** `golem eval` checks arithmetic, one tool call
  with no result round trip, vision, throughput and a needle (`eval.rs`). It never runs a
  multi-step tool loop, never grades a coding answer, and never speaks `/v1/messages` -
  the API Claude Code uses.
- **Choosing a model is a vendor's number.** `tune --auto` ranks by the SWE-bench score a
  vendor reports (`config/library.ini`); a model outside the library has no score at all.
  Whether a model fits is learned by loading it (`hub::budget` is arithmetic on file
  size); the GGUF header already holds what the KV cache costs per token.
- **The tuner pays full price for answers it already has.** A point over the VRAM wall
  ran the whole bench although it is excluded whatever it measures; every candidate
  learned the context slope again; nothing is reused between runs.

## The order, and why

Speed first, because every later item multiplies tuner and gate runs. Agent evaluation
before discovery, because a discovery that ranks by a vendor's score recommends what the
vendor says, not what works on this machine: the local quality signal has to exist before
anything can be ranked by it. Profiles after both, since a profile is a set of floors and
targets over the same measurements. Shared results last: they need a row format that
has stopped moving and a quality signal worth sharing.

## 1. Tuning and gating, as fast as the measurements allow

Every knob the tuner turns is a load-time flag, so the cost is model loads and the bench
inside each; the levers are fewer loads and less bench per load.

- **In:** a load over the wall stops after the warmup (`bench --wall`): the reading is
  the point, and the decode runs and the prefill are not spent. A context search starts
  from the slope an earlier candidate measured with the same engine, `ubatch-size` and
  drafting (`ContextSearch::with_slope`), which spares the doubling load.
- **Next:** reuse measured points across runs, keyed by the model file's hash, the engine
  build, the flags and the desktop's VRAM at the start; one `/tokenize` in the needle
  instead of two; `tune --auto`'s cheap pass measures the frontier only.
- **Measured** on the reference box, 2026-10-29, same session: the previous build's tune
  took 22 min 51 s and 9 loads to n35 at 249856 tokens; this one 19 min 08 s and 10 loads
  to the same split at 247808, then 18 min 06 s and 8 loads to `ub1024` at 262144. Every
  run passed the 200K gate. Three of the four frontier loads stopped after the load.
  The second run spent one load on a context probe read once loaded, before the bench
  adds its ~70 MiB; the context search now adds the seed's own difference to such a
  reading, which brings that run's prediction back to one load.
- **In, the ranking's noise.** The third run ranked `ub1024` first by 4% on decode - past
  a fixed 3% tie band - where the quiet-desktop pair measured the two as tied and
  `ub2048` 57% faster on prefill. Its two benches were 4% and 5% wide (best run over
  median), so the gap sat inside their own spread. The tie is now that spread, held
  between 3% and 8% (`tune::tie_band`), and costs no load. The evening's run on a busy
  desktop ranked n35 first with `ub1024` "tied within 6.0%" and passed the gate (639 t/s
  prefill, 33.4 at depth) in 16 min 32 s - and 11 loads.
- **In, the context search near the wall.** Four of those loads went to contexts within
  26K tokens of the seed (262144, 242688, 238592, 237568), every one over the wall by
  3-466 MiB, and the search ended on the seed's 236544: the desktop moved more between
  loads than the last two predictions could buy. A prediction now has to promise more
  context than 128 MiB of VRAM is worth at the fitted slope (`READING_NOISE_MIB`); on
  that run's readings the search stops after 242688, two loads sooner.
- **Measured** on the reference box, 2026-10-30: ten loads and 30 min 43 s to the verdict
  `n-cpu-moe 36` at 250880 tokens. The gate passed - needle at 655 t/s prefill and 33.8 t/s
  at depth, the agent suite ten of eleven on both APIs - but the verdict is not the served
  setup's: 36 experts on the CPU where the served preset has 35, 250880 tokens where it has
  248832, and the served setup solves eleven of eleven. The desktop held 895 MiB of the
  card at the start and RAM free fell to 0.4 GB of 22.2 by the gate: `n-cpu-moe 35`
  measured 53.6 t/s there and 11343 MiB, over the run's 11264 MiB wall, where on 2026-10-29
  the same split fitted at 11115 MiB. Backed out - the preset, the overlays and the winner
  are restored from `.local/backup/2026-10-30-tune/`. The "done when" is not met: ten loads
  is over seven, and the split it ranked was set by this desktop's memory, not by the card.
- **Done when:** the reference box's `golem tune ornith-1.5-35b` reaches the same verdict
  in at most seven loads and measurably less wall-clock than the same day's run of the
  previous build, and the 200K gate passes on the result.

## 2. Agent evaluation

The question a coder's setup must answer is whether an agent gets work done on it, and
how long the work takes. Three ways to measure it:

| Approach | For | Against |
|---|---|---|
| Real agents (Claude Code, OpenCode, Codex) in Docker on a fixed repo | the real thing | minutes per task, needs Docker and network, nondeterministic, the agent's version moves the number |
| Public suites (BFCL for tool calls, Aider's polyglot, SWE-bench Lite) | comparable with published numbers | a Python harness, hours per run, contamination |
| Golem's own tasks against an in-memory workspace | seconds per task, deterministic, pure in `golem-core`, both APIs | smaller than real work |

Golem's own tasks are the gate-grade signal; a Docker run of a real agent is the
occasional cross-check. Each task is a starting workspace (a few files in memory), a
request, the tools an agent has (read, list, write, search, run the tests - answered by
the harness, never by a shell), and a grader that reads the final workspace. The harness
runs the loop through golem's own port, once over `/v1/messages` and once over
`/v1/chat/completions`, and records per task: solved or not, turns, tool calls that
failed to parse or broke their schema, wall time, prompt and generated tokens. The first
tasks: fix a failing test from its output, rename across files, find a fact in a long
file with search, recover from a tool error, two independent calls in one turn.

- **In:** eleven tasks: the first four above (all but the parallel calls), and seven that
  ask more - a bug whose obvious fix breaks another module, so the tests have to run
  twice; a setting at the end of a chain of included files, with a profile nothing
  includes as a decoy; a translation to complete with its placeholders kept and the file
  still JSON; a one-line fix in a 1,200-line file, which `edit_file` makes without
  rewriting the file; a total over sixty rows of a table to the cent, refunds left out; a
  rename a text search does not finish, because one caller builds the name from pieces;
  a fix in one of two functions alike to the letter, in a long file. `golem eval --agent [anthropic|openai]` runs them and records a row
  of kind `agent` (solved, turns, calls, malformed calls, seconds, tokens and the tasks it
  missed, per API); `golem eval --long` runs them after the needle, so the gate's row
  carries the same, and the tuner's gate holds each API to `TUNE_MIN_AGENT` (75%). The
  console shows the newest verdict as a seal beside the gate's. The first four alone,
  2026-10-29: the 0.5B solved 1 of 4 on both APIs, the 9B and the 35B all 4.
- **First questions for it:** flags whose effect is on the work rather than the speed.
  The engine preserves reasoning across turns by default (its log says so at every load),
  which grows each turn's prompt in an agent loop; `--no-reasoning-preserve` saves the
  prefill, and only the suite can say what it costs in solved tasks. The same holds for
  the KV cache's quantization and for `--image-min-tokens 1024`, which the engine
  recommends for Qwen-VL grounding.
- **Measured** on the reference box, 2026-10-30, quiet desktop, at the 16384-token cap:
  the 35B solves all eleven on both APIs (61 turns, 78 calls, no malformed call) in
  452-486 s and 230K prompt tokens; the 9B solves ten, on both APIs, in 700-713 s and 210K.
  The task between them is the rename past a built name: the 35B finds the caller that
  looks the function up as `getattr(users, "get_" + kind)` and solves it in 8 turns
  (210 s); the 9B renames every `get_user` it finds, rewrites that lookup so the other
  kinds break, and its last turn runs out at 16384 tokens (440 s). The 9B also writes half again as many tokens (30.7K against 19.5K): it reasons
  longer for less. At the old 4096 cap the two had tied at ten, the 35B's miss a turn
  cut mid-thought.
- **Measured, the flags**, 2026-10-30, on the 35B at the 16384-token cap:
  `--no-reasoning-preserve` solves eleven of eleven on both APIs for the same 230K prompt
  tokens as the served preset. The flag reaches the engine - its load echoes
  `--no-reasoning-preserve` and the init line turns from "enabled by default" to "consider
  enabling it via --reasoning-preserve" - but b11379 renders no history reasoning into the
  prompt either way: a two-turn request whose assistant turn carries forty short sentences
  as `reasoning_content` measures 913 prompt tokens under both presets. `--image-min-tokens
  1024` changes nothing either: the vision check reads the picture's colour with it and
  without it, so it is not adopted. The KV cache answers the same way: `q4_0` solves the
  same eleven of eleven as `q8_0` (446-477 s against 452-486 s) and buys nothing, and
  `f16`, whose cache is twice `q8_0`'s and whose preset holds 131072 tokens where the
  served one holds 248832, solves ten. The served preset keeps `q8_0`.
- **Next:** tasks that separate models nearer the 35B, since it solves every one. A missed
  task leaves its calls, last answer and changed files in `logs/agent/`, and `--task NAME`
  reruns one task.

## 3. Discovery: fit before download, rank by local results

- Read the GGUF header straight from the Hub (a range request: the metadata sits at the
  start of the file) and compute the weights per device and the KV cache per token for
  the card at hand, before a byte of weights is fetched. `golem pull` and the console
  say "fits at 200K", "fits at 64K" or "does not fit" from that arithmetic, marked as a
  prediction until a load confirms it.
- Rank candidates by the agent suite where a model has been measured, by the vendor score
  only where it has not, and say which.
- **Done when:** for the models in `results/results.jsonl`, the predicted context is within
  5% of the context the tuner reached, and the console's pull suggestions name a fit for
  this machine.

## 4. Profiles

A profile is a set of floors and a target, over measurements the tuner already takes:

| Profile | Context target | Ranks by |
|---|---|---|
| Coding agent | the 200K gate | agent suite, then decode at depth |
| Local chat | 32K | decode, then first-token time |
| Maximum performance | the smallest the model is useful at | decode |

- **Done when:** `golem tune --profile chat` and the console's profile choice produce a
  gated setup per profile, and switching profiles is a restart, not a new tune.

## 5. Portable reports, then shared results

- `golem report --card` writes one self-contained file per setup: the host's anonymous
  hardware slug, the engine build, the model file and its hash, the flags, and every
  measurement with its seal (proven, measured, predicted).
- Sharing comes after: a card is checked by its hash and its numbers against the same
  hardware class before it means anything to another machine.
- **Done when:** a card from the reference box re-reads into a second checkout's Results
  as that host's rows, with nothing personal in it.

## The app

The mark (`golem_core::mark`), the icon and version block in the executable, the
console, the screen's header, `golem app` - a frameless window on the console, its title
bar drawn by the page, with a tray whose spark shows the engine's state, and `golemw.exe`
to start it with no console window (`--features desktop`) - the Start-menu and desktop
shortcuts `golem setup` makes on Windows, and a bar for a pull, a tune or an eval in the
console, the screen and the terminal are in. The console is dark only, says what each
number is worth (proven by the gate, measured, or not measured), and gives each agent its
exact setup. What is left:

- **The Linux tray.** The desktop build (`golem-linux-x86_64-desktop`, WebKitGTK from the
  system) opens its window and the page's title bar under WSLg; WSLg has no tray, so the
  tray and its menu have not been seen on a Linux desktop yet.

## Release

`.github/workflows/golem-bin.yml` builds and tests on the three platforms whenever a push
to `master` touches the code or a file the binary compiles in, and attaches the binaries to
a `v*` tag, each with nothing to install beside it: `golem.exe` and `golemw.exe` with the
WebView2 loader and the C runtime linked in, a static (musl) `golem` for Linux, a Linux
desktop build that uses the system's WebKitGTK, and macOS. Tagging is a push, and it waits
for the go-ahead.

## Candidates, when the GPU is free

The lineup is two presets, both gated at 200K on b11379: `ornith-1.5-35b` for code and
`ornith-1.5-9b` (Q4_K_M, MTP off) for everything else, with the 0.5B smoke model beside
them ([models.md](models.md)). A model joins only by beating one of them on this machine
and holding the 200K gate. What is still open, from
[engines-and-models.md](engines-and-models.md):

- **The next llama.cpp build.** b11879 and b11936 both fail `load-mode = mlock` and load
  under `mmap+mlock`; b11879 decodes ~11% slower than b11379, b11936 4-10% slower, and
  b11755 loads but decodes slower at depth. Each new release gets the `measure` skill's
  build trial under `mmap+mlock`, and the adapter's `build` moves when one wins.
- **Strata 0.2.7.** A routing prior for cards of 13 GiB or less (+15-37% decode on a 3060
  by its own notes) may close the gap that kept 0.2.6 at half llama.cpp's decode.
- **`load-mode = none` for the coder.** With experts on the CPU the engine says a load
  without mmap may run faster; the served preset pins with `mlock`. Measured 2026-10-30 on
  the served split, `mlock` / `none` / `none` / `mlock`: decode best 60.8 and 60.9 against
  60.5 and 60.8 t/s, median 60.5 and 60.2 against 59.8 and 60.6, prefill 854 and 854
  against 854 and 825. `none` wins nothing, so `mlock` stays and no gate run is owed.
- **`fit` on a box that never tuned the presets.** A checkout served on another machine
  would let llama.cpp place the model instead of carrying this box's split. b11379 refuses:
  the shipped presets' `[*]` sets `n-gpu-layers = 99`, and fit aborts beside it
  (`common_fit_params: failed to fit params to free device memory: n_gpu_layers already set
  by user to 99, abort`), after which the load hangs rather than failing. Dropping that
  global line as well is untested, and what fit places for a model with its experts in RAM
  is unknown.
- **MTP for the everyday model.** Ornith-1.5-9B decodes 76-78 t/s with its MTP head, but
  the draft's own KV cache stops the context at 209920 tokens on 12 GB. A build or flag
  that shrinks the draft's cache below the wall at 223232 tokens would be a ~40% faster
  everyday model.

## Web search, phase 2: pages that need JavaScript

`golem_web_fetch` reads a page's HTML without running it, and a page that builds its
content with JavaScript says so and gives its description
([architecture.md](architecture.md#web-search)). The next step renders such pages in a
headless browser, on any host that has one: golem finds a Chromium-family browser where
each OS installs it, starts it headless on a temporary profile, and reads the rendered DOM
over the DevTools protocol on a loopback socket. A host with no browser, a container among
them, keeps today's fetch. The same private-address check applies to every request the
page makes.

## Smaller items

- The AMD path (`GPU_BACKEND = "rocm"` or `"vulkan"`) has never been through the eval
  gate: an RX 7800 XT on Windows is the first box to run it. Its `eval --long 200000` on
  both backends decides which one `auto` gives AMD.
- macOS is experimental: the first real Mac's `golem tune` and `eval --long 200000` decide.
- `scripts/cargo.sh` picks the built binary from `target/<profile>/` when no target triple
  is configured; that branch (Linux and macOS checkouts) has not been run by hand.
- mintty and raw conhost have not been walked by hand with the screen.

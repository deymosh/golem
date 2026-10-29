# Roadmap

What is left, in order. The design is in [architecture.md](architecture.md) and
[tuning.md](tuning.md); this page is only the work.

Golem's job is to make the best local setup for a machine easy to find, prove and use: it
knows the engines, fetches models from Hugging Face, measures them on the machine at hand,
tunes them to the card and the RAM, gates a setup before it becomes a default, and hands
any agent the endpoint. "Best" means best at the work an agent does on that machine - a
tokens-per-second figure is one input, not the answer.

## Where it stands

Checked against the code and `bench/results.jsonl` (176 rows on the reference box):

- **What holds up.** One binary owns the engine (`service.rs`); every decision is a pure,
  tested function in `golem-core` (558 tests); a row records host, build, flags and
  model file, so a number is always traceable; nothing is adopted without the 200K gate.
  The tuner places a model in eight or nine loads where a grid spent seventeen.
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

- **In:** `golem eval --agent [anthropic|openai]` runs four of the tasks above (all but
  the parallel calls) and prints a verdict per task and a `RESULT agent_<api>=solved/total`
  line per API; it records no row yet. First runs, 2026-10-29: the 0.5B solves 1 of 4 on
  both APIs, the 9B and the 35B all 4 with no malformed call, in 32-51 s per API. It
  separates a toy model from a working one and not yet the two served models: harder
  tasks (a bug that needs two files read, a test the model must run twice, a long
  context to work in) come next.
- **First questions for it:** flags whose effect is on the work rather than the speed.
  The engine preserves reasoning across turns by default (its log says so at every load),
  which grows each turn's prompt in an agent loop; `--no-reasoning-preserve` saves the
  prefill, and only the suite can say what it costs in solved tasks. The same holds for
  the KV cache's quantization and for `--image-min-tokens 1024`, which the engine
  recommends for Qwen-VL grounding.
- **Done when:** `golem eval --agent` runs the suite on both APIs, writes one row per run
  with the solve rate and the times, the 35B and the 9B are measured on the reference box,
  and the gate includes the suite at a floor the adopted models pass.

## 3. Discovery: fit before download, rank by local results

- Read the GGUF header straight from the Hub (a range request: the metadata sits at the
  start of the file) and compute the weights per device and the KV cache per token for
  the card at hand, before a byte of weights is fetched. `golem pull` and the console
  say "fits at 200K", "fits at 64K" or "does not fit" from that arithmetic, marked as a
  prediction until a load confirms it.
- Rank candidates by the agent suite where a model has been measured, by the vendor score
  only where it has not, and say which.
- **Done when:** for the models in `bench/results.jsonl`, the predicted context is within
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
  without mmap may run faster; the served preset pins with `mlock`. One alternating pair
  of benches decides it.
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

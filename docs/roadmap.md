# Roadmap

What is left, in order. The design is in [architecture.md](architecture.md) and
[tuning.md](tuning.md); this page is only the work.

Golem's job is to make the best local setup for a machine easy to find, prove and use: it
knows the engines, fetches models from Hugging Face, measures them on the machine at hand,
tunes them to the card and the RAM, gates a setup before it becomes a default, and hands
any agent the endpoint. "Best" means best at the work an agent does on that machine - a
tokens-per-second figure is one input, not the answer.

## Where it stands

Checked against the code and `results/results.jsonl` (278 rows on the reference box):

- **What holds up.** One binary owns the engine (`service.rs`); every decision is a pure,
  tested function in `golem-core` (642 tests in the workspace); a row records host,
  build, backend, flags and model file, so a number is always traceable; nothing is
  adopted without the 200K gate.
- **The gate measures work.** Beside the needle and decode at depth, `golem eval --long`
  runs twelve agent tasks on `/v1/messages` and `/v1/chat/completions` and holds each to
  a 75% floor (section 2).
- **Choosing a model reads this machine first.** Whether a model fits is predicted before
  the download from its header and the engine's own sizing; `tune --auto` ranks by the
  agent suite where the machine has run it and by a vendor's score only where it has not
  (section 3).
- **The tuner stopped paying twice for one answer.** A point over the VRAM wall stops
  after its load, a context search starts from an earlier candidate's slope, a measured
  point is kept while the desktop holds still, and a gate keeps the suite of the same
  setup: a tune on a steady desktop is two loads and nine minutes (section 1).

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
- **In, the rest of a run.** The needle counts its haystack once (`/tokenize` on the
  first pass, the ratio corrected, the needle's `prompt_n` the count of what was read:
  200010 tokens either way). `tune --auto`'s cheap pass measures each preset's frontier
  and nothing after it (`tune --frontier`). And a gate keeps the agent suite of an
  earlier gate on the same model file, build, backend and answering flags at no more
  context (`tune::kept_agent`): the suite asks at temperature 0, so the split and the
  context move its speed, not its answers.
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
- **In, kept points.** Every measured point is kept in `data/tuned/<host>/points.jsonl`
  under the model file, the engine build and backend, the candidate's flags and the
  bench's size, with the desktop's share at the run's start; a later run takes it without
  a load while the desktop has moved no more than 128 MiB, or while its over-the-wall
  reading, moved by the desktop's difference, is still over (`tune::reuse`, docs/tuning.md).
- **Measured** on the reference box, 2026-11-03, one session, the same build: with nothing
  kept, 8 loads and 1724 s to `n-cpu-moe 35` at 245760 tokens (desktop 800 MiB); the same
  tune again, 7 loads and 1681 s to `n-cpu-moe 35` at 242688 (desktop 466 MiB) - the
  desktop moved 334 MiB, so only n31 and n33, still over the wall at the moved reading,
  were kept; the rest were loaded again. Both gates passed (667-670 t/s prefill, 35-36 t/s
  at depth, the agent suite eleven of eleven on both APIs). A run that keeps the frontier
  and the ubatch and spec points spends one load: the chat profile's first run took six
  kept points. The gate is never kept, and its agent suite alone took 843 s of run B's
  1681 (416 and 427 s for the two APIs).
- **Measured** on the reference box, 2026-11-03, later the same day, this build, desktop
  364 MiB: `golem tune ornith-1.5-35b` took 2 loads and 516 s to `n-cpu-moe 35` at 248832
  tokens - the served setup's own split and context. The frontier and the refine points
  were kept from run B (the desktop 102 MiB from its 466), the context search loaded
  248832 once, and the gate kept the chat gate's suite (11/11 on both APIs at 32768) and
  ran the needle alone: PASS, 673 t/s prefill, 35.5 at depth.
- **Done when:** the reference box's `golem tune ornith-1.5-35b` reaches the same verdict
  in at most seven loads and measurably less wall-clock than the same day's run of the
  previous build, and the 200K gate passes on the result. Met: 2 loads and 516 s against
  run B's 7 loads and 1681 s, the gate passed. The first run on a moved desktop still
  measures its points; what it no longer spends is the suite.

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

- **In:** twelve tasks: the first four above (all but the parallel calls), and eight that
  ask more - a bug whose obvious fix breaks another module, so the tests have to run
  twice; a setting at the end of a chain of included files, with a profile nothing
  includes as a decoy; a translation to complete with its placeholders kept and the file
  still JSON; a one-line fix in a 1,200-line file, which `edit_file` makes without
  rewriting the file; a total over sixty rows of a table to the cent, refunds left out; a
  rename a text search does not finish, because one caller builds the name from pieces;
  a fix in one of two functions alike to the letter, in a long file; and a number worked
  out across files - the client's own retry loop says how many waits there are, the base
  config gives the numbers, the production file overrides one and a cap cuts the last
  two, so each misreading is another total. `golem eval --agent [anthropic|openai]` runs them and records a row
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
- **Measured, the twelfth task**, 2026-11-03, on the served presets, both APIs: the 35B
  solves eleven of twelve and the 9B eleven of twelve - but not the same eleven. The 35B
  misses the backoff task on both APIs: it reads every file, then spends its turns asking
  to "verify by simulating" until the limit without ever answering (the workspace has no
  tests, and it asks for them eight times). The 9B solves it and still misses only the
  rename past a built name. So the suite has a task the 35B does not solve, and the two
  models' scores meet at 11/12 - the separation the first eleven could not give.
- **Next:** more tasks in that class - a number or a decision the tools cannot hand over,
  where the failure is to keep calling instead of answering. A missed
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
- **Built, the rank** (`tune --auto`, `results::agent_share`): a model whose file this
  machine ran the suite on is ranked by its newest run's share on the worse API - at the
  floor (`TUNE_MIN_AGENT`) or over, ahead of every model never measured; under it, behind
  them, whatever a vendor says. The models never measured keep the vendor score, then
  the size. Each line of the ranking names what it read (`agent 100%`, `vendor 79`,
  `unscored`). On the reference box's rows, 2026-11-03: the 35B and the 9B both 11 of 12
  (the twelfth task each misses a different one), and the 0.5B unscored - no row of its
  file carries the suite.
- **What the header holds.** The 35B's metadata ends 10.9 MB into the file (the
  tokenizer's vocabulary is most of it) and its 753 tensor descriptions follow, so one
  16 MB range request covers a model of this class. Its KV geometry is scalar keys:
  `qwen35moe.attention.head_count_kv = 2`, `key_length = value_length = 256`, and
  `full_attention_interval = 4` - of its 40 layers only every fourth keeps a KV cache, the
  rest a fixed-size recurrent state; block 40 is the MTP head, which keeps one more when
  drafting. The 9B has the same shape with 4 KV heads; the 0.5B (`qwen2`) keeps a cache
  in all 24 layers; `gguf::read` and `gguf::read_bytes` keep them.
- **The rule, written before measuring.** Predicted context = the largest multiple of 256
  tokens whose cache fits in what the card has left, capped at the trained context:
  (the tuner's VRAM wall − the tensors the split leaves on the card − the compute buffer)
  ÷ the cache per token. The cache per token is the layers that keep one (plus the MTP
  layer when drafting) × KV heads × (key + value length) × the cache type's bytes per
  element (q8_0: 34 bytes per 32). The compute buffer is the engine's own
  `compute buffer size` lines for that architecture and `ubatch-size`, read once from a
  load's log; a model whose architecture has never loaded is a prediction with that
  allowance taken from the nearest measured one, and says so. The split is the tuned
  `n-cpu-moe` when one exists, else the fewest experts on the CPU that leave a 200K cache
  room. If a prediction misses the tuned context by more than 5%, the rule is wrong - the
  rule changes, written down, before the next comparison.
- **In:** `gguf::read` and `gguf::read_bytes` (the first bytes of a file, as a range
  request fetches them) keep the KV heads and widths, the layers that keep a cache (every
  layer, the `full_attention_interval`-th, or a per-layer head count's non-zero ones), the
  MTP layers and every tensor's size from its offset; `golem_core::fit` turns them into
  the cache per token, the weights a split keeps on the card and the context left.
- **Measured, the first comparison**, 2026-10-31, from the 35B's own header: at the served
  split (`n-cpu-moe 35`, 248832 tokens, drafting) the tensors on the card are 4314 MiB
  and the cache 2840 MiB (11968 bytes a token), so on the tuner's 11264 MiB wall the
  rule leaves 4.1 GB to the compute buffer - several times what one holds, so the rule
  as written predicts far past the tuned context and is wrong. The next rule, to check
  against a load's memory breakdown before it is compared: the compute buffer includes
  the output logits, `ubatch-size` x the vocabulary (248320 tokens here) x 4 bytes -
  1.9 GiB at `ubatch-size 2048` - for the model and again for the MTP head, which the
  header gives; the CUDA context and the desktop's share of the card are the wall's.
- **Measured, the second comparison**, 2026-11-03, from b11379's `llama-fit-params -v`,
  which builds the context and reserves its graphs without allocating anything on the
  card: the logits term holds where it dominates and fails where it does not, so the
  second rule is wrong too. The 0.5B at `ubatch-size 2048`: compute buffer 1194 MiB
  against 1187 of logits. The 35B at the served split without drafting: weights 3792 MiB
  (the header's 4314 less the MTP block's 522, which a load that does not draft leaves
  out), cache 2582 and recurrent state 63, compute 1992 against 1940 of logits. The 9B at
  its served `ubatch-size` (512, the engine's default): compute 1336 MiB against 485 of
  logits - the buffer is the largest of the graphs the engine reserves, not a sum, and
  another graph is the largest there. Its tuned context (262144) is the trained one, so
  the comparison cannot fail on it whatever the compute term.
- **What the measurement leaves: ask the engine.** `llama-fit-params` gives the same
  breakdown from the header alone: on a file holding the 35B's first 16 MB and a sparse
  hole up to its real size (16 MB on disk), it printed what the 21.7 GB file did, 8429 =
  3792 + 2644 + 1992 MiB, in 0.6 s. With no `-c` it fits the context itself (262144 for
  the 35B at `n-cpu-moe 35` without drafting, within its 1024 MiB `--fit-target`
  margin). b11379's fit-params has no MTP option, so drafting adds what golem reads from
  the header - the MTP block's weights and its layer's cache - and its compute buffer,
  which only a drafting load's breakdown gives. Assuming that equals the model's 1992
  MiB, on the 11264 MiB wall with the recurrent state counted, the 35B predicts 254208
  tokens against the tuned 248832 (+2.2%).
- **Measured, the drafting load**, 2026-11-03: the served 35B (b11379, drafting,
  `n-cpu-moe 35`, 248832 tokens, `ubatch-size 2048`) at full verbosity holds 4314 MiB of
  weights (the MTP block in), a 2582 MiB q8_0 cache, 251 MiB of recurrent state (four
  copies, `3 rs_seq`, against one without drafting) and a 1866 MiB compute buffer; its
  MTP draft context adds a 486 MiB cache - in f16, whatever the target's type: one layer
  x 2 heads x 512 values x 2 bytes, 2048 bytes a token - and a 1132 MiB compute buffer.
  That is 10631 MiB, exactly fit-params' 8429 plus the MTP weights (522), three more
  recurrent copies (188), the draft's cache and compute, and the target's buffer 126 MiB
  smaller than without drafting; the card read 10745 MiB over the desktop's 772, so 114
  MiB of a load lie outside its breakdown. The assumed 1992 MiB for the draft's compute
  was wrong, and so was its cache type.
- **Decided: the engine sizes the fit.** The formula failed twice; the pinned build's
  `llama-fit-params` gives every term but the draft context's, so discovery asks it and
  adds only what it cannot say (`golem_core::fit::engine_context`): the MTP weights and
  the draft's f16 cache from the header, the recurrent state's extra copies, 1006 MiB of
  compute for drafting (1132 less 126, the one drafting load measured, at `ubatch-size
  2048`), and the 114 MiB outside the breakdown. The prediction is the largest context
  that keeps the card under the tuner's wall with the desktop's share as nvidia-smi reads
  it now - so it moves by 81 tokens a MiB of desktop on the 35B (12928 bytes a token
  drafting), and says which desktop it assumed.
- **Built** (`discover.rs`): one range request (16 MB, then 4x up to 256 MB while the
  header runs past it) into `.local/fit/`, kept as a sparse file of the model's size
  (`fsutil sparse setflag` and `setrange` on Windows); fit-params at the trained context
  on the split to serve - a preset's tuned `n-cpu-moe`, else the fewest expert layers in
  RAM that leave a 200K cache room by the header's tensor sizes - then again at the
  answer, because the compute buffer grows with the context (the 9B's card reading
  climbed 22.6 KB a token from 64K to 256K against a 17.4 KB cache). A preset is sized as
  it serves (its cache types, `ubatch-size` - the engine's 512 when unset - and drafting
  only with `spec-type = draft-mtp`); a new model as `pull` writes it. `golem pull
  owner/repo` prints the fit before the download; the console's Models list shows it on
  a preset not downloaded yet (`POST api/fit`). Three headers take 49 MB on disk for 27
  GB of models; a prediction takes 3-12 s, most of it the range request.
- **Checked against the tunes**, 2026-11-03, at 0.8 GB of desktop: the 35B predicts
  224512 tokens on `n-cpu-moe 35` (the split the search also picks), the 9B (MTP off,
  `ubatch-size 512`) 252928, the 0.5B its trained 32768. Each tune's reading of the card
  gives the desktop it had: the 35B's 11249 MiB at 248832 against this load's 10745 is
  504 MiB, at which the rule predicts 250112 (+0.5%); the 9B's 11217 MiB at 262144
  against fit-params' 10550 + 114 is 553 MiB, at which it predicts the trained 262144
  (the tuned one). Today's desktop would hold neither tuned context under the wall, and
  the prediction says so.
- **Done when:** for the models in `results/results.jsonl`, the predicted context is within
  5% of the context the tuner reached at the desktop it had (met for the three, above),
  and the console's pull suggestions name a fit for this machine (built), and the rank by
  local results is built. Left: a second drafting architecture's draft compute - every
  model in `config/library.ini` is of the Qwen3.5 family, so it waits for one that drafts
  on another architecture.

## 4. Profiles

A profile is a set of floors and a target, over measurements the tuner already takes:

| Profile | Context target | Ranks by |
|---|---|---|
| Coding agent | the 200K gate | agent suite, then decode at depth |
| Local chat | 32K | decode, then first-token time |

A third, maximum performance at the smallest context a model is useful at, was built and
measured, and is dropped: 32K is that context (below).

- **Done when:** `golem tune --profile chat` and the console's profile choice produce a
  gated setup per profile, and switching profiles is a restart, not a new tune.
- **Built** (`golem_core::profile`, docs/tuning.md "Profiles"): `coder` (the default, the
  200K gate) and `chat` (32768 tokens, a 29491-token needle).
  `golem tune <preset> --profile chat` measures every point at the profile's context - a
  split found at the coder's 236544 tokens is not the one 32K leaves room for - gates it, and writes
  `<preset>@chat.ini` beside the coder's overlay; `PROFILE` (`golem profile <name>`, or
  the Settings tab) switches the effective preset's overlays and a restart serves it.
  The ranking is the tuner's for every profile: decode within the floors, a tie to the
  faster prefill.
- **Measured** on the reference box, 2026-11-03: the chat tune of the 35B found
  `n-cpu-moe 25` at 32768 tokens - 65.9 t/s decode and 769 t/s prefill against the coder
  split's 56.7 and 637 in the same session - in 8 loads and 982 s, the context point
  kept from the frontier's; the gate passed (needle 1338 t/s prefill, 59.4 at depth, the
  agent suite 11/11 on both APIs in 262-266 s). `golem profile chat` and a restart
  served `--ctx-size 32768 --n-cpu-moe 25`; `golem profile coder` (and the same through
  `POST api/settings`) put the served preset back, the 9B's and 0.5B's sections untouched.
- **Measured, the maximum-performance profile**, 2026-11-03, the 35B at 16384 tokens
  (gate needle 14745): the frontier at `n-cpu-moe 25` (62.8 t/s, 11040 MiB) and n24 51
  MiB over the wall - the split chat's 32768 places - with n26 ranked first on the tie
  (62.0 t/s, 718 prefill). All four candidates passed the needle (52.1-63.4 t/s at
  depth) and none the gate: the agent suite's `follow-the-includes` sends a 20292-token
  request, which the engine refuses at 16384, and the suite stopped there. Two things
  follow. A request over the context is the setup's answer, so the suite now grades it
  as a missed task and goes on (`agent::over_context`). And no agent setup is smaller
  than 32768 - Claude Code's first prompt alone is about 27K - so "the smallest the
  model is useful at" is the chat profile's context, and `max` is dropped: on this card
  it bought no split, and a profile no setup can pass is not one to offer.
- **Decided, the table's rankings.** The agent suite asks at temperature 0, so every
  placement of one model answers it alike - which is what lets a gate keep it (section
  1) - and ranking a model's splits by it ranks nothing: it ranks models, which is
  `tune --auto`'s ranking (section 3). A chat prompt's first token waits on its prefill,
  and the tuner's decode tie already goes to the faster prefill, so the chat profile's
  "decode, then first-token time" is the ranking it has. Decode at depth is measured
  once, by the gate; ranking a coder's splits by it would spend a 200K needle (about
  five minutes) on every point, and the gate's floor holds it instead.

## 5. Portable reports, then shared results

- `golem report --card` writes one self-contained file per setup: the host's anonymous
  hardware slug, the engine build, the model file and its hash, the flags, and every
  measurement with its seal (proven, measured, predicted).
- Sharing comes after: a card is checked by its hash and its numbers against the same
  hardware class before it means anything to another machine.
- **Done when:** a card from the reference box re-reads into a second checkout's Results
  as that host's rows, with nothing personal in it.
- **Built** (`golem_core::card`): `golem report --card [--model NAME]` writes one JSON
  Lines file per setup (host, model file, engine build and backend) into `results/cards/`:
  a header with the host slug, the build, the model file and its SHA-256 (its `.sha256`,
  hashed and written beside it when missing), the newest row's flags and the seal (proven,
  failed or measured - a prediction is not a row, so no card holds one), then the setup's
  rows exactly as `results.jsonl` holds them, which is all a public results file holds.
  `golem report --import <card>` checks every row parses and belongs to the header's host
  and setup and appends the ones the checkout lacks. Checked 2026-11-03: the 35B's six
  setups gave six cards; the served one (113 rows, proven) imported into an empty home as
  113 rows of `rtx-3060_12g_32g_i7-12700kf`, its RESULTS.md that host's section, and a
  second import added none - done. Sharing - a card checked by its hash and its numbers
  against the same hardware class - is the next step.

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
  b11755 loads but decodes slower at depth. b12065 (2026-11-03) passes the 200K gate with
  5-7% more prefill and decode at depth, 3.5% less short decode and ten of eleven agent
  tasks against b11379's eleven (engines-and-models.md), so it is not adopted either.
  Each new release gets the `measure` skill's build trial under `mmap+mlock`, and the
  adapter's `build` moves when one wins.
- **Strata 0.2.7.** A routing prior for cards of 13 GiB or less (+15-37% decode on a 3060
  by its own notes) may close the gap that kept 0.2.6 at half llama.cpp's decode. Not
  measured: its best claim, +37% on 0.2.6's 34.9 t/s, is about 48 t/s against
  llama.cpp's 61-63 on the same box, so it cannot win on decode, and the trial costs the
  Coder pack again (~64.5 GB). It is measured when a release claims enough to close that
  gap.
- **`load-mode = none` for the coder.** With experts on the CPU the engine says a load
  without mmap may run faster; the served preset pins with `mlock`. Measured 2026-10-30 on
  the served split, `mlock` / `none` / `none` / `mlock`: decode best 60.8 and 60.9 against
  60.5 and 60.8 t/s, median 60.5 and 60.2 against 59.8 and 60.6, prefill 854 and 854
  against 854 and 825. `none` wins nothing, so `mlock` stays and no gate run is owed.
- **`fit` on a box that never tuned the presets.** A checkout served on another machine
  would let llama.cpp place the model instead of carrying this box's split. b11379 refuses:
  the shipped presets' `[*]` sets `n-gpu-layers = 99`, and fit aborts beside it
  (`common_fit_params: failed to fit params to free device memory: n_gpu_layers already set
  by user to 99, abort`), after which the load hangs rather than failing. Measured
  2026-11-03: the shipped `config/models.ini` with that global line dropped and the 35B's
  `n-cpu-moe 36` replaced by `fit = on`, at its 236544 tokens - a box that never tuned -
  loads, and fit leaves its 1 GiB margin: 10569 MiB on the card, 57.2 t/s decode median and
  848 prefill, against the tuned split's 63.0-63.2 and 850-876 the same morning. Its 200K
  gate passes: needle at 651 t/s prefill and 36.4 at depth, the agent suite ten of eleven
  on both APIs (the rename past a built name, its last turn cut at the token cap). And a
  tuned overlay, which sets `fit = off` and carries no `n-gpu-layers` of its own, loads the
  same without the global line - the engine's `auto` puts every layer on the card: the
  served split read 11220 MiB and 61.8 t/s against 11203 and 63.0-63.2 with it. So the
  shipped presets drop `n-gpu-layers = 99`, and the coder's section says `fit = on` where
  it carried this box's `n-cpu-moe 36`; a tune's overlay still sets the split.
- **MTP for the everyday model.** Ornith-1.5-9B decodes 76-78 t/s with its MTP head, but
  the draft's own KV cache stops the context at 209920 tokens on 12 GB. A build or flag
  that shrinks the draft's cache below the wall at 223232 tokens would be a ~40% faster
  everyday model. Measured 2026-11-03: b11379's `spec-draft-type-k/v = q8_0` (the
  draft's cache type) does not: the tune drafted at 77.4 t/s and stopped at 194560
  tokens (196608 read 29 MiB over the wall, desktop 329 MiB), short of the gate as
  before; MTP off at 262144 passed it again (944 t/s prefill, 22.4 at depth, the agent
  suite 10/11 on both APIs), and the preset is unchanged.

## Web search, phase 2: pages that need JavaScript

`golem_web_fetch` reads a page's HTML with curl, and a page that builds its content with
JavaScript is rendered in a headless browser on any host that has one: golem finds a
Chromium-family browser where each OS installs it, runs it on a throwaway profile, and
reads the rendered DOM ([architecture.md](architecture.md#web-search)). A host with no
browser, a container among them, says the page needs JavaScript and gives its
description. The same private-address check applies to every request the page makes.

- **Decided, 2026-10-31: the system's Chromium-family browser, not Lightpanda.**
  Lightpanda (AGPL-3.0, Zig, V8, CDP) ships Linux and macOS binaries and no Windows one -
  its README sends Windows users to WSL - and it sends usage telemetry unless
  `LIGHTPANDA_DISABLE_TELEMETRY=true` is set; golem would have to download and run a
  third-party binary to use it. Windows 11 ships Edge, most desktops have Chrome, Linux
  desktops a Chromium, so the browser is already there: nothing to install, nothing to
  bundle. Golem works out of the box or not at all, so WSL ends it.
- **Measured** on the reference box, 2026-10-31, eight real pages, headless
  (`--headless=new --dump-dom --virtual-time-budget=10000`, a fresh profile each,
  background networking, sync, component updates and pings off), visible text after
  scripts and styles are dropped: today's fetch reads 0-37 characters of crates.io,
  Apple's Swift documentation, a Bluesky profile and a Spotify artist page (crates.io
  answers curl with a 404); the browser reads 2.9K, 41.9K, 18.7K and 10.3K, each with
  the page's own content in it. LMArena answered the browser with a Cloudflare challenge
  page ("Un momento…") and gave neither path its content. Docker Hub and a GitHub releases page
  read the same both ways. Chrome took 1.9-2.6 s and peaked at 520-550 MiB of working
  set (all its processes summed); Edge 3.0-9.4 s and 630-1000 MiB, and it tried to start
  its updater (`LaunchUpdateCmdElevatedAndWait`) whatever the flags said. So Chrome,
  Chromium or Brave is preferred where one is installed, and Edge is the fallback every
  Windows box has.
- **Built** (`golem_core::browser`, `render.rs`): a page curl's HTML says needs
  JavaScript is rendered by one headless browser process per fetch, one render at a
  time, on a profile under `.local/browser/` removed after it, read with `--dump-dom`
  - the rendered DOM, without a DevTools client to write - and handed to the same
  HTML-to-Markdown path; the process tree is killed at `WEB_TIMEOUT`. Every request
  goes through a loopback proxy golem runs for that render
  (`--proxy-server=http://127.0.0.1:<port>`, `--proxy-bypass-list=<-loopback>` so even
  the loopback goes through it, QUIC off, WebRTC kept to proxied connections): golem
  resolves each host the page asks for, refuses this machine and the local network, and
  connects to the address it checked; a plain-HTTP request is sent with
  `Connection: close`, so a reused connection cannot reach a second host unchecked. No
  browser, `WEB_BROWSER = "off"`, a failed render or one with no more text keeps the
  static page and its note.
- **Checked** on the reference box, 2026-11-03, Chrome 156 through the proxy
  (`tests/render_live.rs`, ignored by default): crates.io 1962 characters, Apple's Swift
  documentation 36560, a Bluesky profile 16778, a Spotify artist page 3986, each with
  its page's own phrase, in 1.6-5.7 s; no profile and no browser process left behind.
  Pointed at a loopback listener, the browser never reached it and the proxy counted
  the refusal. Through the service, the 0.5B's fetch of crates.io was rendered (2.3 s)
  and logged as `rendered by chrome`; GitHub's releases page, whose HTML holds its
  text, was not rendered. The agent-level web check on the 35B is still to run.

## Smaller items

- **AMD out of the box.** On Windows golem installs the ROCm runtime the llama.cpp ROCm
  zip lacks - hipBLAS, rocBLAS and their kernels, from AMD's pinned ROCm 7.2.1 wheel - and
  `auto` falls back to Vulkan, saying why, only when that fetch fails; a Vulkan engine
  serves `load-mode = mlock` as `mmap+mlock`, which b11379 needs to load the 35B there
  ([amd-rocm.md](amd-rocm.md)). Proven on the NVIDIA reference box: the fetch, the hash,
  the placement, every import resolving, and the 35B's load, bench and gate under Vulkan.
  AMD itself is unproven: an RX 7800 XT's `--list-devices` showing `ROCm0`, then its
  `eval --long 200000` on both backends, decide whether ROCm works and which one `auto`
  gives AMD.
- macOS is experimental: the first real Mac's `golem tune` and `eval --long 200000` decide.
- `scripts/cargo.sh` picks the built binary from `target/<profile>/` when no target triple
  is configured; that branch (Linux and macOS checkouts) has not been run by hand.
- mintty and raw conhost have not been walked by hand with the screen.

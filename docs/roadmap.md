# Roadmap

What is left, in order. The design is in [architecture.md](architecture.md) and
[tuning.md](tuning.md); this page is only the work.

## 1. Settle the tuner's verdict on a quiet desktop

The first full run (2026-10-24, [tuning.md](tuning.md#measured)) reached a gated verdict in
eight loads and adopted n-cpu-moe 35, `ubatch-size 1024`, ctx 262144: 62.3 t/s decode but
505 t/s prefill, where the October setup (`ub2048`, ctx 249344) prefilled 878. It ran on a
busy desktop. Next, on a quiet one: a paired bench of the two setups (`ub1024` vs `ub2048`
at the adopted split and context), and decide whether the ranking should weigh prefill -
a Claude Code session pays it on every first prompt. The context prediction reached the
trained ceiling in one load, so it was not stressed against the wall; a `--ctx-ladder` run
is only worth its loads if a later tune lands a context below the trained one.

The 0.5B smoke model cannot answer this: it is dense (the split search ends in one load)
and its 32K trained context caps the search long before the card's wall.

## 2. Release

`.github/workflows/golem-bin.yml` builds and tests on the three platforms and attaches a
binary per OS to a `v*` tag. A fresh clone has no binary until then and builds one with
`./scripts/cargo.sh build`. Tagging is a push, and it waits for the go-ahead.

## 3. Presentation

A tray icon and a logo: presentation over the service and `api/state`, which are already
plain data. Both need things the repository does not carry yet - a GUI toolkit dependency
and an asset - so they are decided deliberately, not added in passing. Starting the
service at log-on already works without them (`schtasks` or a shortcut to
`golem service --up`, [windows-native.md](windows-native.md)).

## 4. Candidates, when the GPU is free

From [engines-and-models.md](engines-and-models.md): the everyday-slot pair (Ornith-1.5-9B,
Gemma 4 12B QAT), a paired retest of llama.cpp b11755, and Strata's 0.2.x engine line
against the 200K requirement. Each is a preset or an adapter, a paired bench and the eval.

## Smaller items

- The AMD path (`GPU_BACKEND = "rocm"`, the rocm release assets, the rocm image) has never
  been through the eval gate here, and `golem status` / the tuner read VRAM through
  `nvidia-smi` only.
- The first `golem up` after a boot starts the service itself and can still hold a piped
  caller's handle on Windows; clearing the inherit flag around that one spawn closes it.
- mintty and raw conhost have not been walked by hand with the screen.

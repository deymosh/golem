# Roadmap

What is left, in order. The design is in [architecture.md](architecture.md) and
[tuning.md](tuning.md); this page is only the work.

Golem is the app a person installs instead of a model runner: it knows several engines,
fetches models from Hugging Face, measures them on the machine at hand, tunes them to the
card and the RAM, and gates a setup before it becomes a default. The items below serve
that.

## 1. Settle the tuner's verdict on a quiet desktop

The quiet-desktop pair ([tuning.md](tuning.md#measured)) found `ub1024` and `ub2048` tied
on decode with `ub2048` 55-58% faster at prefill, and `ub2048` over the wall at the trained
context. The ranking now compares spills within one `ubatch-size` and breaks a decode tie
on prefill. Next: one `golem tune ornith-1.5-35b` on a free desktop, so the tuner itself
weighs `ub2048` at the largest context under the wall against `ub1024` at the trained one,
and gates the winner.

## 2. Models by name, from Hugging Face

Hugging Face is the one model source. Adding a model takes one name -
`golem pull owner/repo[:quant]`, or a name from the shipped library - and golem works out
the rest from the Hub's API: the files and their sizes (split shards grouped), the
architecture, trained context and parameter count, the base model and its
`generation_config.json` sampling defaults, a vision projector or a draft. With no quant
given it picks the largest one this machine can hold.

Each fact about a model then lives in one place:

| File | Holds | Written by |
|---|---|---|
| `config/models.ini` | llama-server flags, and the `; Download:` lines `up` fetches from | golem, on pull; the tuner overlays its own |
| `data/models/<file>.json` | repo, revision, quant, files, size, base model, licence, date | golem, beside the file |
| `config/library.ini` | curated models: repo, quant, role, vendor score and its source | shipped with golem |

`golem pull owner/repo[:quant]` writes the first two (`golem_core::hub`); the library is
compiled into the binary, `golem pull <name>` fetches a library model by name, and `tune
--auto` ranks by its scores. Still to do: the console's pull box, and ranking a model
outside the library by measured speed and size, since the Hub publishes no benchmark
scores.

## 3. The app

The mark (`golem_core::mark`), the icon and version block in the executable, the
redesigned console, the screen's header, and `golem app` - a frameless window on the
console, its title bar drawn by the page, with a tray whose spark shows the engine's state,
and `golemw.exe` to start it with no console window (`--features desktop`) - are in.
What is left:

- **A shortcut.** `golem setup` could put `golemw.exe` in the Start menu and on the
  desktop; today a shortcut is made by hand.
- **Linux.** The desktop build needs WebKitGTK on the build machine; CI builds Linux
  without it until the runner installs it.
- **Progress for long steps** in the console: a pull or a tune shows its log, not yet a
  bar.

## 4. Release

`.github/workflows/golem-bin.yml` builds and tests on the three platforms and attaches a
binary per OS to a `v*` tag. A fresh clone has no binary until then and builds one with
`./scripts/cargo.sh build`. Tagging is a push, and it waits for the go-ahead.

## 5. Candidates, when the GPU is free

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

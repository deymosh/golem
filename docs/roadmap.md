# Roadmap

What is left, in order. The design is in [architecture.md](architecture.md) and
[tuning.md](tuning.md); this page is only the work.

Golem is the app a person installs instead of a model runner: it knows several engines,
fetches models from Hugging Face, measures them on the machine at hand, tunes them to the
card and the RAM, and gates a setup before it becomes a default. The items below serve
that.

## 1. Models by name, from Hugging Face

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
--auto` ranks by its scores; the console and the screen pull by the same name. A model
outside the library has no score to read - the Hub publishes none, and the model cards
checked (Ornith 1.5 9B and 35B) carry no `model-index` either - so `tune --auto` ranks it
after every scored one, by parameters, then the context and the decode its cheap sweep
reached, and a model under the floors is no candidate at all.

## 2. The app

The mark (`golem_core::mark`), the icon and version block in the executable, the
redesigned console, the screen's header, `golem app` - a frameless window on the
console, its title bar drawn by the page, with a tray whose spark shows the engine's state,
and `golemw.exe` to start it with no console window (`--features desktop`) - and the
Start-menu and desktop shortcuts `golem setup` makes on Windows, and a bar for a pull, a
tune or an eval in the console, the screen and the terminal are in. What is left:

- **The Linux tray.** The desktop build (`golem-linux-x86_64-desktop`, WebKitGTK from the
  system) opens its window and the page's title bar under WSLg; WSLg has no tray, so the
  tray and its menu have not been seen on a Linux desktop yet. `golem setup` on a
  graphical session installs the libraries and writes the applications-menu entry.

## 3. Release

`.github/workflows/golem-bin.yml` builds and tests on the three platforms and attaches the
binaries to a `v*` tag, each with nothing to install beside it: `golem.exe` and
`golemw.exe` with the WebView2 loader and the C runtime linked in, a static (musl)
`golem` for Linux, a Linux desktop build that uses the system's WebKitGTK, and macOS. A
fresh clone has no binary until then and builds one with `./scripts/cargo.sh build`. Tagging is a push, and it waits for the go-ahead.

## 4. Candidates, when the GPU is free

From [engines-and-models.md](engines-and-models.md): the everyday-slot pair (Ornith-1.5-9B,
Gemma 4 12B QAT), a paired retest of llama.cpp b11755, and Strata's 0.2.x engine line
against the 200K requirement. Each is a preset or an adapter, a paired bench and the eval.

## Smaller items

- The AMD path (`GPU_BACKEND = "rocm"`, the rocm release assets, the rocm image) has never
  been through the eval gate here, and `golem status` / the tuner read VRAM through
  `nvidia-smi` only.
- mintty and raw conhost have not been walked by hand with the screen.

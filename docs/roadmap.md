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

`.github/workflows/golem-bin.yml` builds and tests on the three platforms whenever a push to
`master` touches the code or a file the binary compiles in, and attaches the binaries to a `v*` tag, each with nothing to install beside it:
`golem.exe` and `golemw.exe` with the WebView2 loader and the C runtime linked in, a static
(musl) `golem` for Linux, a Linux desktop build that uses the system's WebKitGTK, and
macOS. The test matrix passes on all three platforms, and v0.1.0 (2026-10-28) shipped the
four assets. A fresh clone has no binary until a release and builds one with
`./scripts/cargo.sh build`. Tagging is a push, and it waits for the go-ahead.

## 4. Candidates, when the GPU is free

The lineup is two presets, both gated at 200K on b11379: `ornith-1.5-35b` for code and
`ornith-1.5-9b` (Q4_K_M, MTP off) for everything else, with the 0.5B smoke model beside
them ([models.md](models.md)). A model joins only by beating one of them on this machine
and holding the 200K gate. What is still open, from
[engines-and-models.md](engines-and-models.md):

- **The next llama.cpp build.** b11879 and b11936 both fail `load-mode = mlock` and load
  under `mmap+mlock`; b11879 decodes ~11% slower than b11379, b11936 4-10% slower, and
  b11755 loads but decodes slower at depth. Each new release gets the `measure` skill's
  build trial under `mmap+mlock`, and the adapter's `build` moves when one wins. Where
  between b11755 and b11879 `mlock` started failing is worth pinning down for an upstream
  report; each probe is one load that may reset the display driver.
- **Strata 0.2.7.** A routing prior for cards of 13 GiB or less (+15-37% decode on a 3060
  by its own notes) may close the gap that kept 0.2.6 at half llama.cpp's decode. It
  needs the ~64.5 GB Coder pack again, and is measured as another server, like 0.2.6.
- **MTP for the everyday model.** Ornith-1.5-9B decodes 76-78 t/s with its MTP head, but
  the draft's own KV cache stops the context at 209920 tokens on 12 GB. A build or flag
  that shrinks the draft's cache below the wall at 223232 tokens would be a ~40% faster
  everyday model.
- Flint fine-tunes: Flint itself was rejected (context capped at 88064 tokens on 12 GB,
  the needle failed twice); a variant with a smaller KV footprint would be worth a tune.

## Smaller items

- The AMD path (`GPU_BACKEND = "rocm"` or `"vulkan"`, their release assets, the rocm
  image) has never been through the eval gate: an RX 7800 XT on Windows is the first box to
  run it. Its `eval --long 200000` on both backends decides which one `auto` gives AMD.
- macOS is experimental: the card reader (`machine::gpu`) has no macOS branch, so `golem
  tune` refuses there and the VRAM readings stay empty. Apple Silicon's memory is shared;
  the branch reports the machine's memory and the share Metal may use
  (`recommendedMaxWorkingSetSize`) as the card. Then a Mac's `eval --long 200000` decides.
- `scripts/cargo.sh` picks the built binary from `target/<profile>/` when no target triple
  is configured; that branch (Linux and macOS checkouts) has not been run by hand.
- mintty and raw conhost have not been walked by hand with the screen.

# golem-rs — the binary

One `golem` binary that owns everything: the commands, the measurement, the tuner, the
console, the TUI, the engine and the service that owns it. There is no shell behind any of
it — the shell stack was deleted on 2026-10-20 once this binary replaced every gate it had.
The plan, phase by phase, is [docs/plan-portable-binary.md](../docs/plan-portable-binary.md).

```
crates/
  golem-core/   the testable core: JSON, results.jsonl, INI, GGUF, RESULT lines
  golem/        the binary
```

## Build and test

```sh
./scripts/setup-rust-toolchain.sh   # once per machine
./scripts/cargo.sh test
```

`setup-rust-toolchain.sh` gives this checkout a compiler without installing anything
system-wide:

- **Windows** — a pinned mingw-w64 in `.local/toolchain/` (~264 MiB, no installer, no
  admin, checksum verified) plus the **GNU-hosted Rust toolchain** (`rustup toolchain
  install stable-x86_64-pc-windows-gnu`), and a per-directory `rustup override` pointing
  this checkout at it. Rust's usual Windows requirement is the MSVC linker from Visual
  Studio Build Tools (~2 GB, admin); this avoids it entirely.
- **Linux / macOS** — the system compiler; only the Rust side is checked.

The GNU *host* matters, not just the GNU target: build scripts always compile for the
host, so with an MSVC host any dependency that has one (tokio and axum pull in several)
still needs the MSVC linker. A GNU host links everything with mingw-w64.

**Build through `./scripts/cargo.sh`, not plain cargo.** The dependency tree runs
`dlltool` from the toolchain's `bin` by name, and cargo's `[env]` section cannot put PATH
in front of a build script — the wrapper does, and forwards every argument. (Plain
`cargo test` works too if that `bin` is already on your PATH.)

A build also drops the binary at the repository root as `golem.exe` (git-ignored): a
binary sitting next to `bench/` and `data/` is exactly the shape a release has, so
`./golem.exe report --md` works on the checkout's own files.

**CI is unaffected and still builds MSVC** (`.github/workflows/golem-bin.yml`), which is
what a released Windows binary should be. The local GNU build is for testing; it is
self-contained (`+crt-static`, no mingw DLLs beside it) but it is not byte-identical to
the MSVC release.

## Where the binary keeps its files

A release is one executable someone downloaded, so nothing about the repository layout
can be assumed. The rule is one sentence: **the home is `GOLEM_HOME` if set, otherwise
the directory the binary lives in.** Everything the binary needs lives under it, and is
created on first run if missing — `bench/`, `data/models`, `data/cache`, `data/tuned`,
`logs/`, and the page template, which is compiled into the binary and written out the
first time it is needed (an existing copy is never overwritten, so editing it wins).

That single rule covers both shapes: a release unpacked anywhere populates its own
folder, and a dev build at the repository root uses the checkout's files by virtue of
sitting next to them.

## Status

Every command is this binary's, and there is no shell left to fall back to. Phases 0-6 per
[docs/plan-portable-binary.md](../docs/plan-portable-binary.md): the scaffold and CI; the
read-only core (JSON, `results.jsonl` with its size classes and best-per-configuration
reduction, the `RESULT`-line contract, the GGUF header, the INI reader and merge, both report
views); the console and its Models tab; the controls; the tuner; the TUI; and the switchover,
which ended with the launchers deleted rather than handed anything.

`up` resolves the command line, installs the pinned build when it is absent (`release.rs`,
over curl and `%SystemRoot%\System32\tar.exe` on Windows — no new crate, and Git's GNU tar on
PATH cannot read a zip), fetches the model the preset needs before starting (`pull.rs`'s
downloader, so `up` and `pull` fetch the same bytes by the same rule), spawns the server,
writes the pid and polls `/health` with a deadline and an early exit. `down` kills the process
tree and waits for the card; `restart` is the two. An engine whose adapter says it installs
itself — Strata — is refused by name: there is nothing to hand it back to.

**The engine belongs to `golem service`** (`service.rs`). `up`, `down`, `restart` and
`status` are executed by that process, and the CLI is a *client* of it: it sends a verb over a
loopback socket and the output streams back a line at a time, ending with the exit code. Two
things fall out of that. A client that spawns nothing cannot hand its caller's handles to a
long-lived process, which is what closes the piped-`up` problem that `std`'s handle
inheritance causes on Windows; and the engine is a child of the service, so it goes when the
service does instead of being orphaned by a killed run. `golem service --up` is the same
service with the engine started on the way in, for a container's PID 1, which has nobody to
send it a verb. A request may carry `GOLEM_*` overrides, set for that one verb and put back
after, which is how the tuner points a candidate restart at its own preset.

The recorder is `golem-core::record` plus `src/facts.rs`: the row a bench or an eval writes to
`bench/results.jsonl`, verified byte-identical to what the shell recorder wrote for the same
capture against the same live engine — which is what `golem bench --replay <capture>` is for.

Two things worth knowing before changing it: **the configuration is `golem.toml`**, written
from the compiled-in `golem.toml.example` on first run and never overwritten after (a key the
binary reads comes from there, and `.env` is gone); and `golem` is a bin-only crate, so a
module a tested module uses has to be declared in `tests/*.rs` as well — see the `#[path]`
block at the top of each.

The binary is git-ignored and so is the `golem.toml` beside it, so a fresh clone builds its
own with `./scripts/cargo.sh build`; a release is the executable plus the config it writes.
The container ([../Dockerfile](../Dockerfile)) is the same binary with the pinned engine baked
in and `golem service --up` as PID 1.

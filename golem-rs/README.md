# golem-rs — the portable binary

One `golem` binary that owns the TUI, the console, the results database and the tuner,
so the shell stack is needed only as engine drivers. The plan, phase by phase, is
[docs/plan-portable-binary.md](../docs/plan-portable-binary.md); a script is deleted only
when this binary replaces its gate.

```
crates/
  golem-core/   the testable core: JSON, results.jsonl, INI, GGUF, RESULT lines
  golem/        the binary (phase 0: --version and help)
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

Phases 0-6, per [docs/plan-portable-binary.md](../docs/plan-portable-binary.md): the
scaffold and CI; the read-only core (JSON, `results.jsonl` with its size classes and
best-per-configuration reduction, the `RESULT`-line contract, the GGUF header, the INI
reader and merge, both report views); the console (`golem web`'s page, `results.jsonl`,
`api/state`, `api/logs`, `api/run`, and the Models tab's `api/models`,
`api/addmodel`, `api/delmodel`, `api/delpreset`); the controls (`golem status`,
`up`/`down`/`restart`) driving the shell launchers; `golem tune <preset>`, which
measures through those same launchers and decides with `golem-core`'s tuner; `golem
tui`; and the switchover, where `./golem` and `golem.cmd` hand it every command it
owns.

Every command the shell used to have is the binary's now - `logs`, `pull`, `bench`,
`eval`, `claude`, `env`, `menu`, `setup`, `install` and `tune --auto` alongside the
phases above, and since 2026-10-20 `up` / `down` / `restart` as well: the binary
installs the pinned engine build from its GitHub release, spawns the server with its
own `--log-file`, writes the pid file and waits for `/health` itself. What is left of
the shell is `web status` / `web stop`, the launchers' helpers, the no-binary
fallbacks, and the two cases `up` hands back - docker mode, and an engine that
installs itself (Strata's git-clone-and-Python setup).

The recorder is `golem-core::record` plus `src/facts.rs`: the row a bench or an eval
writes to `bench/results.jsonl`, verified byte-identical to what
`scripts/record-result.sh` wrote for the same capture against the same live engine -
which is what `golem bench --replay <capture>` is for.

The two things worth knowing before changing it: the binary **orchestrates** (the
scripts stay the engine drivers, and phase 3's rule is that a script is deleted only
when the binary replaces its gate), and `golem` is a bin-only crate, so a module a
tested module uses has to be declared in `tests/*.rs` as well - see the `#[path]`
block at the top of each.

What the clean-machine walk found, and what the next plan is: a fresh clone has no
`golem.exe` (the binary is git-ignored), so the missing piece is a **release asset**.
The measurement no longer needs a shell - `bench`, `eval` and `tune` are `bench.rs`,
`eval.rs` and `tune.rs`, and `scripts/benchmark-ai.sh`, `scripts/eval-model.sh`,
`scripts/record-result.sh`, `scripts/tune.sh` and `scripts/tune-auto.sh` were deleted
on 2026-10-20 under the migration gate. The scripts that remain are the launchers'
helpers and the no-binary fallbacks.

The launchers are ported for Windows and native mode: `golem up` resolves the command
line, installs the pinned build when it is absent (`release.rs`, over curl and
`%SystemRoot%\System32\tar.exe` - no new crate, and Git's GNU tar on PATH cannot read a
zip), fetches the model the preset needs before starting (`pull.rs`'s downloader, so
`up` and `pull` fetch the same bytes by the same rule), spawns the server, writes the
pid and polls `/health` with a deadline and an early exit; `down` kills the process tree
and waits for the card; `restart` is the two. Docker, Strata and single-model mode with
no preset keep the shell launcher.

The live paired start has run: `golem up` and `start-windows.ps1` produce the same
`/props`, the same pid file and the same banner, and in a scratch clone `up --dry-run`
matches `-DryRun` flag for flag for both single-model shapes. What keeps `./golem` and
`golem.cmd` owning their `up`/`restart` arms is one measured gap - on Windows the engine
inherits the caller's stdout pipe, so a *piped* `up` returns only when the engine stops
([plan-service.md](../docs/plan-service.md) is the fix) - plus docker, which
[plan-deploy-modes.md](../docs/plan-deploy-modes.md) argues should stop being a mode at all. The plan
is -
[docs/plan-launchers.md](../docs/plan-launchers.md) has them in order. After that the
last `.env` keys move to `golem.toml` and there is no shell left anywhere.

# Plan: the deploy mode goes away

Status: **done** (2026-10-20). The key is gone from the binary, and the image
is built and verified: one `Dockerfile` carries the binary, the pinned engine
(baked in by `golem install`) and `golem service --up` as PID 1, and
`compose.yml`'s ~70-line shell entrypoint and its digest pin are retired with
it. `DEPLOY_MODE` stays in `.env.example` only because the shell launchers
still read it - it leaves with them (the deletion pass,
[plan-launchers.md](plan-launchers.md)).

## As built, and what the container forced

The Dockerfile is a builder stage (plain `cargo build --release`, as CI does
it) and two runtime stages - `nvidia/cuda:13.4.1-runtime-ubuntu24.04` and
`rocm/dev-ubuntu-24.04` - with `FROM runtime-${GPU_BACKEND}` picking between
them. `GOLEM_HOME=/golem`, a baked `.env` (`LOCAL_AI_HOST=0.0.0.0`,
`GPU_BACKEND` from the build arg, router mode), the adapter and presets
copied in, `RUN golem install` baking the engine, and volumes for
`data/models`, `bench/`, `logs/` and `data/tuned` - `.local` stays with the
image, because the image is the engine's home.

Four things the container made real, all fixed on the way:

- **`golem service` now stops on SIGTERM**, not only Ctrl+C: `docker stop`
  sends SIGTERM to PID 1, and a Linux PID 1 with no handler ignores it, so
  every stop would have burned the grace period to a SIGKILL that never ran
  `down`. Measured: `docker stop` returns in 1s with the service's own
  shutdown lines and exit code 0.
- **The engine child is reaped once healthy**: the parent outlives it now,
  and a dropped handle meant a zombie - one `kill -0` still answers for, so
  `down` would poll a corpse. One blocked thread waits on the engine for its
  lifetime.
- **`GOLEM_LOCAL_AI_HOST`** overrides the file's bind host: the mounted
  `.env` is a host-side setting (`127.0.0.1` keeps the LAN out), but a
  container publishes its port and its engine must bind `0.0.0.0`. compose
  sets the override in the process environment, the same idiom the tuner's
  three overrides use.
- **`linux_runtime_cuda` matched nothing**: the Linux cudart archive carries
  the build tag (`cudart-llama-b11379-bin-ubuntu-...`) where the Windows one
  does not. And the CUDA base image lacks `libgomp1`, the one library the
  llama.cpp release needs that neither base carries - without it the server
  dies with exit 127 before it can log.

Verified on the box (Docker 29.8.2): the build installs b11379-cuda from the
two ubuntu archives; a container with the models mounted starts the router
and `/health` answers through the published port with **no model loaded**
(the cheap check); the 0.5B (`qwen2.5-0.5b-instruct`) serves both
`/v1/chat/completions` and `/v1/messages` at 0.5B speeds; `golem status`
inside the container sees the engine and the service; the card is back at
desktop after the stop.

## What the key was

`DEPLOY_MODE` takes three values, and they are not three of the same kind of thing:

| value | what it actually does |
|---|---|
| `windows` | downloads the **pinned prebuilt release** into `.local/engines/<name>/<build>-<backend>/` and runs that |
| `native` | **builds llama.cpp from source** into `.local/bin/` and runs that (`install` is refused in `windows` mode, which is the tell) |
| `docker` | runs the **engine in a container** and golem on the host |

So `windows` and `native` are not two operating systems - they are two **engine acquisition
strategies**, and both of their names come from where each was first needed rather than from
what each does. `windows` means "take the artifact somebody else built"; `native` means
"build it here". The operating system is a third axis that neither name mentions, and on a
Linux host the pair reads as nonsense: `native` is the only one implemented there.

## What actually varies, once the binary runs the show

- **The platform.** Not a setting. An adapter can name its assets per platform - the same
  way it already names `windows_asset_cuda` and `windows_asset_rocm` - and the binary picks
  the pair for the system it is on. Nothing here is worth a key.
- **Release or build.** A real choice, and the only one left. The pinned release is what the
  200K gate evals, it is what almost everyone wants, and it is a download instead of a
  ten-minute compile. A local build is for a machine the release does not cover: a GPU
  architecture or ROCm target it has no binary for, or a driver newer than its CUDA. That is
  what `golem install` is for, and `AMDGPU_TARGETS` is the proof that it is a real need
  rather than a preference.
- **Host or container.** Not golem's business at all - see below.

## The shape that replaces it

`DEPLOY_MODE` disappears, and with it `--windows` / `--native` / `--docker` on the front
door. `up` and `install` take the pinned release for the platform they are on, and the
adapter names the assets per platform. Everything is detection plus adapter data, which is
where it belonged.

**Releases only, on purpose.** A source build needs git, cmake and a compiler, and the pin
is the artifact the 200K gate was adopted on - a build from today's upstream is a different
binary that the gate does not describe. `golem install` therefore means "put the pinned
build on this machine", which is also what a container image needs to bake the engine in.

`ENGINE` stays as it is: it picks which adapter, and an adapter is a real choice.

**Both halves have landed in the binary** (2026-10-20). The shell launchers still read
`DEPLOY_MODE` themselves, so the key stays in `.env.example` until they are deleted - the
rule is unchanged: a key moves when the binary owns every reader.

## Docker is packaging, not a mode

`DEPLOY_MODE=docker` runs the *engine* in a container and golem on the host:

- `compose.yml` carries a **~70-line shell entrypoint that re-implements the flag list** -
  the router branch, the single-model branch with `--ctx-size`, `--n-gpu-layers`,
  `--jinja`, `-fa`, the cache types, `--load-mode`, `SPEC_TYPE`, `DRAFT_FILE`,
  `EXTRA_ARGS`, and the shared `--host/--port/--threads/--parallel/--metrics/--api-key`.
  That is a **third copy of the adapter's translation**: `engines/llama.cpp/engine.ini` is
  the first, `start-windows.ps1` the second, and `start-local-ai.sh` says so out loud -
  *"Keep these flags in sync with the docker entrypoint in compose.yml"*.
- Each vendor's image is pinned **by digest**, gated by the 200K eval, so the engine's
  version lives in two places: `build` in the adapter and a digest in `compose.yml`.
- `./golem` maps `GPU_BACKEND` to `COMPOSE_PROFILES`; `up`, `restart`, `down`, `logs` and
  `status` each branch on the mode.

The binary on the host can *measure* a container (the port is published, so `bench` and
`eval` work), but it cannot *drive* one the way it drives a native engine: `up` and
`restart` still go through the shell launcher, and a tune's restart-per-candidate has to
travel through compose and a changed environment.

## The direction

Stop treating docker as a deploy mode. Ship one image with the golem binary in it, set the
entrypoint so golem does everything it already does, and let the mode disappear - because
the mode was only ever *where golem runs*, and golem does not need to know.

That is right, and it is a bigger simplification than it looks:

- **the third flag translation disappears**, and the "keep in sync" hazard with it;
- **one pin instead of two** - the adapter's `build`, which the eval gate already governs;
- **`tune`, `bench`, `eval` and `pull` work inside the container**, because the thing that
  measures and the thing that serves are the same process again;
- **the deployment is the image**: `docker compose up -d`, and the host needs no golem at
  all. `DEPLOY_MODE=docker` stops meaning anything, because the mode was only ever *where
  golem runs*, and golem does not need to know;
- **the container's PID 1 owns the engine**, so `docker stop` stops it - the same
  "everything goes when golem goes" the user asked for on Windows, for free.

## The catch, and why this comes after the service

`up` returns as soon as `/health` answers. That is deliberate - a human typed it and wants
their prompt back - and it means `ENTRYPOINT ["golem", "up"]` is a container that **exits**
the moment it is healthy (`restart: unless-stopped` would then loop it).

So the image cannot be `up`. It needs a process that owns the engine and **stays alive**:
start it, hold it, stop it on the way out. That is exactly `golem service` - the same
process that fixes the piped `up` on Windows, and the same one a tray app would talk to.
Three problems, one fix, and the service is the one to build first.

## What it needs

- **`golem service`** ([plan-service.md](plan-service.md)): a foreground owner of the
  engine, which is what PID 1 has to be.
- **Asset keys per platform.** The adapter says `windows_repo`, `windows_asset_<backend>`,
  `windows_runtime_<backend>`. The pinned release ships the Linux equivalents -
  `llama-b11379-bin-ubuntu-cuda-13.4-x64.tar.gz` and
  `cudart-llama-b11379-bin-ubuntu-cuda-13.4-x64.tar.gz` - and they are **`.tar.gz`, which
  GNU tar reads**. The Windows path needs `%SystemRoot%\System32\tar.exe` because GNU tar
  cannot read a zip; the Linux path needs nothing but the `tar` that is already there.
  `release.rs` is otherwise the same code.
- **A Dockerfile, and one image per vendor from it.** CUDA and ROCm both have to work, and
  the engine half of that already exists: the adapter names its assets per backend
  (`windows_asset_cuda` / `_rocm` today, and the Linux keys the same way), and the pinned
  release ships both `llama-b11379-bin-ubuntu-cuda-13.4-x64.tar.gz` and
  `llama-b11379-bin-ubuntu-rocm-10.0-x64.tar.gz`. So the Dockerfile takes
  `--build-arg GPU_BACKEND=cuda|rocm`, bakes the matching engine in, and the two compose
  profiles keep choosing between them exactly as they do now. The runtime wiring is
  unchanged and stays the runtime's business - `gpus: all` plus the NVIDIA container
  toolkit for CUDA, `/dev/kfd` + `/dev/dri` for ROCm.
  Baking the engine in is the choice worth making: a container start should not be a
  download. The alternative - one image carrying only the binary, installing the engine on
  first run into a volume - is what a bare `docker run` of that image would do, and it is
  also what `up` does on a host, so both paths exist either way; the shipped image should
  be the baked one. ROCm has no separate runtime zip in either case (there is no
  `windows_runtime_rocm` and no Linux equivalent), because its libraries come from the
  driver.
- **A Dockerfile**: the binary plus the engine baked in (so a container start is not a
  download), `GOLEM_HOME=/golem`, and volumes for `data/models`, `bench/`, `logs/` and
  `data/tuned`. The GPU wiring stays the runtime's business - `gpus: all` plus the NVIDIA
  container toolkit, or `/dev/kfd` + `/dev/dri` for ROCm - exactly as `compose.yml` does it
  today.
- **Retiring `DEPLOY_MODE=docker`**, and with it the mode's key in `.env.example`: the
  value disappears rather than gaining a replacement, because there is nothing left for the
  host to switch on. `windows` and `native` stay - they still choose which engine build to
  run.

## What it does not fix

**Speed, on this box.** Docker Desktop's WSL2 GPU-PV costs about half the decode rate
against native Windows (measured; `docs/windows-native.md`). A golem-in-a-container is a
portability and LAN story - "run this on any machine with a GPU" - not a speed one, and on
a Linux host with a real driver it is near-native. The reference box should keep serving
through `DEPLOY_MODE=windows` either way.

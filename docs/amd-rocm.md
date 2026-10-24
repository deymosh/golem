# Running Golem on AMD GPUs (ROCm/HIP)

Everything in this repo defaults to NVIDIA + CUDA, but the same stack runs on AMD GPUs
through llama.cpp's ROCm/HIP backend. Nothing has to be configured on a normal machine: the
GPU vendor is **auto-detected** (`GPU_BACKEND = "auto"`, the default) — NVIDIA resolves to
`cuda`, AMD to `rocm`. Set it explicitly only when detection can't work or guesses wrong
(headless box, remote host, dual-GPU):

```toml
GPU_BACKEND = "rocm"    # or "cuda"; auto detects NVIDIA -> cuda, AMD -> rocm
```

`rocm` then means two things, and they are the same engine either way:

| Where | What `rocm` selects |
|---|---|
| native (Linux, Windows) | the `linux_asset_rocm` / `windows_asset_rocm` release the adapter names — the official `llama-*-bin-ubuntu-rocm-*-x64.tar.gz` or `llama-*-bin-win-rocm-*-x64.zip`, installed by `golem up` or `golem install`. The HIP runtime ships inside the archive, so there is no ROCm equivalent of the separate `cudart` runtime the CUDA asset needs |
| container | the `rocm` build arg: `docker build --build-arg GPU_BACKEND=rocm -t golem:rocm .`, run with `/dev/kfd` and `/dev/dri` passed through instead of `gpus: all` ([compose.yml](../compose.yml), `--profile rocm`) |

## Requirements

- A [ROCm-supported](https://rocm.docs.amd.com/projects/install-on-linux/en/latest/reference/system-requirements.html)
  AMD GPU (RDNA2/RDNA3 discrete cards and most recent ones are fine) with an up-to-date
  driver. On Windows, the Adrenalin driver is enough for the official win-rocm build — no HIP
  SDK install required, and no build tools at all, because golem installs a release.
- Container on **Linux**: nothing beyond the driver — the container maps `/dev/kfd` (compute)
  and `/dev/dri` (display) itself.

### WSL2 / Docker Desktop caveat

ROCm inside WSL2 works only for a handful of card/driver combinations. On a Windows AMD host,
run natively: the win-rocm binary talks to the driver directly, and that is what `golem up`
installs by default there.

## gfx targets

The pinned release is built for the official ROCm targets, so a supported card needs nothing
here. A build of your own — not something golem does any more, since `golem install` fetches
the pinned release rather than compiling — is where `AMDGPU_TARGETS=gfx1100` (RX 7900),
`gfx1030` (RX 6000) and the like come in. APU and lesser variants (gfx1102, gfx1032, ...)
aren't official targets; the usual workaround is `HSA_OVERRIDE_GFX_VERSION` (e.g. `11.0.2`
for gfx1102, `10.3.0` for gfx1032) exported in the environment of the engine — the service
passes its own environment to the engine it starts, so setting it before `golem service` is
what reaches the model. See AMD's ROCm docs.

## Status and the eval gate

**This path is not exercised by the reference machine** (NVIDIA RTX 3060 12 GB): the
rocm release assets and the `golem:rocm` image are pinned from upstream but have never been
through this repo's eval gate. On AMD hardware, right after the first `golem up`, run:

```bash
golem eval --long 200000   # adopt only on PASS — same rule as a CUDA upgrade
```

Also expect the VRAM-headroom notes in [models.md](models.md) to shift: the silent
spill-to-shared-memory threshold that governs `--n-cpu-moe` tuning on the NVIDIA reference box
behaves differently (and not always silently) per AMD driver. Re-run `golem bench` after any
offload change. One known gap: the VRAM line in `golem status` and the header of `golem tui`
comes from `nvidia-smi`, so on AMD it simply stays silent — the health check itself is
vendor-independent.

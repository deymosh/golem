# Running Golem on AMD GPUs (ROCm/HIP)

Everything in this repo defaults to NVIDIA + CUDA, but the same stack runs on AMD
GPUs through llama.cpp's ROCm/HIP backend, in all three deploy modes. Nothing has
to be configured on a normal machine: the GPU vendor is **auto-detected**
(`GPU_BACKEND="auto"`, the default) — NVIDIA resolves to `cuda`, AMD to `rocm`.
Set it explicitly only when detection can't work or guesses wrong (headless box,
remote host, dual-GPU):

```bash
GPU_BACKEND="rocm"    # or "cuda"; auto detects NVIDIA -> cuda, AMD -> rocm
```

Per mode, `rocm` means:

| Mode | What changes |
|---|---|
| `docker` | image `server-rocm` (digest-pinned in [compose.yml](../compose.yml), same eval-gated rule as the CUDA digest) + `/dev/kfd` / `/dev/dri` passthrough instead of `gpus: all` |
| `windows` | first `./golem up --windows` downloads the official `llama-*-bin-win-rocm-*-x64.zip` release instead of the win-cuda one (the HIP runtime ships inside the zip — no cudart equivalent needed) |
| `native` | `install-llama-cpp.sh` builds with `-DGGML_HIP=ON` instead of `-DGGML_CUDA=ON` |

## Requirements

- A [ROCm-supported](https://rocm.docs.amd.com/projects/install-on-linux/en/latest/reference/system-requirements.html)
  AMD GPU (RDNA2/RDNA3 discrete cards and most recent ones are fine) with an
  up-to-date driver. On Windows, the Adrenalin driver is enough for the official
  win-rocm build — no HIP SDK install required.
- `native` mode: the ROCm toolkit on PATH (`hipcc`), exactly like CUDA needs `nvcc`.
- `docker` mode on **Linux**: nothing beyond the driver — the container maps
  `/dev/kfd` (compute) and `/dev/dri` (display) itself.

### WSL2 / Docker Desktop caveat

ROCm inside WSL2 works only for a handful of card/driver combinations. On a
Windows AMD host prefer `DEPLOY_MODE="windows"` (the win-rocm binary talks to the
driver directly); docker mode is the Linux path.

## gfx targets and the odd cards

- `native` builds target the GPU ROCm detects; to pin it (faster configure, CI
  boxes) set `AMDGPU_TARGETS=gfx1100` (RX 7900), `gfx1030` (RX 6000), ...
- APU/lesser variants (gfx1102, gfx1032, ...) aren't official ROCm targets: the
  usual workaround is `HSA_OVERRIDE_GFX_VERSION` (e.g. `11.0.2` for gfx1102,
  `10.3.0` for gfx1032) exported in the environment — in docker mode just set
  it in `.env` (`env_file` passes it into the container). See AMD's ROCm docs.

## Status and the eval gate

**This path is not exercised by the reference machine** (NVIDIA RTX 3060 12 GB):
the `server-rocm` digest and the win-rocm assets are pinned from upstream but
have never been through this repo's eval gate here. On AMD hardware, right after
the first `./golem up`, run:

```bash
./golem eval --long 200000   # adopt only on PASS — same rule as a CUDA upgrade
```

Also expect the VRAM-headroom notes in [models.md](models.md) to shift: the
silent spill-to-shared-memory threshold that governs `--n-cpu-moe` tuning on the
NVIDIA reference box behaves differently (and not always silently) per AMD
driver. Re-run `./golem bench` after any offload change. Small known gap:
`./golem status` prints the VRAM line via `nvidia-smi` — on AMD it simply stays
silent; the health check itself is vendor-independent.

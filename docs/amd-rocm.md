# Running Golem on AMD GPUs (ROCm/HIP)

Everything in this repo defaults to NVIDIA + CUDA, but the same stack runs on AMD GPUs
through llama.cpp's ROCm/HIP backend, or its Vulkan one. Nothing has to be configured on a
normal machine: the GPU vendor is **auto-detected** (`GPU_BACKEND = "auto"`, the default)
from the card's PCI vendor ID — NVIDIA resolves to `cuda`, AMD to `rocm`, Intel to
`vulkan`. No ROCm tool is needed for that: `nvidia-smi` answers when it is there, and
otherwise the Windows display-adapter registry or Linux's `/sys/class/drm` names the card.
Set it explicitly only when detection guesses wrong (headless box, remote host, dual-GPU) or
to try the other AMD backend:

```toml
GPU_BACKEND = "rocm"    # cuda | rocm | vulkan | metal; auto reads the card's vendor
```

`rocm` then means two things, and they are the same engine either way:

| Where | What `rocm` selects |
|---|---|
| native (Linux, Windows) | the `linux_asset_rocm` / `windows_asset_rocm` release the adapter names — the official `llama-*-bin-ubuntu-rocm-*-x64.tar.gz` or `llama-*-bin-win-rocm-*-x64.zip`, installed by `golem up` or `golem install`. The Windows archive carries the HIP runtime (`amdhip64_7.dll`, `amd_comgr.dll`) but **not** the BLAS libraries `ggml-hip.dll` loads (`hipblas.dll`, `rocblas.dll` and its kernels): those come from the AMD ROCm/HIP SDK, below |
| container | the `rocm` build arg: `docker build --build-arg GPU_BACKEND=rocm -t golem:rocm .`, run with `/dev/kfd` and `/dev/dri` passed through instead of `gpus: all` ([compose.yml](../compose.yml), `--profile rocm`) |

### Vulkan

`GPU_BACKEND = "vulkan"` installs the same release's `llama-*-bin-win-vulkan-x64.zip` or
`llama-*-bin-ubuntu-vulkan-x64.tar.gz` (the adapter's `windows_asset_vulkan` /
`linux_asset_vulkan`). It needs nothing beyond the GPU driver and has no gfx target list, so
it is the backend to try when the ROCm build does not load on a card; which of the two is
faster on a given card is for `golem bench` to say. It runs on an NVIDIA card too, which is how the
non-CUDA path — install, start, bench, tune — is exercised on the NVIDIA reference box.

## Requirements

- A [ROCm-supported](https://rocm.docs.amd.com/projects/install-on-linux/en/latest/reference/system-requirements.html)
  AMD GPU (RDNA2/RDNA3 discrete cards and most recent ones are fine) with an up-to-date
  driver. The release's ROCm build covers gfx1100, gfx1101 (RX 7800 XT), gfx1030 and the
  other official targets.
- **Windows, ROCm:** the AMD ROCm/HIP SDK whose version matches the archive's name
  (`win-rocm-10.0` wants SDK 10.0), with its `bin` folder on PATH for `golem service` - the
  engine inherits the service's environment, so set PATH before starting it. The Adrenalin
  driver alone is not enough: without the SDK's `hipblas.dll` and `rocblas.dll` HIP never
  initialises, and llama.cpp runs every layer on the CPU without an error. No build tools
  are needed, because golem installs a release.
- **Windows, Vulkan:** nothing beyond the driver. It is the low-friction path on Windows,
  and the one to use when the SDK is not installed.
- Container on **Linux**: nothing beyond the driver — the container maps `/dev/kfd` (compute)
  and `/dev/dri` (display) itself.

### A build that found no GPU

Before each start golem asks the installed engine `llama-server --list-devices`. The
answer is kept for `golem status` (`device: ROCm0 AMD Radeon RX 7800 XT`) and the console's
status strip. When it lists no device, `up` says so in plain words - for the Windows ROCm
build, which of `hipblas.dll` and `rocblas.dll` it could not find beside the engine or on
PATH - the console shows the model as served "on the CPU only", and `bench`, `eval` and
`tune` refuse to run, so no CPU number is ever filed as the card's.

### The 35B under Vulkan

On b11379 the 35B aborts on load under Vulkan with `load-mode = mlock` (a failed
`GGML_ASSERT` in the lock of its split host buffer); `load-mode = mmap+mlock` in its section
loads it. [windows-native.md](windows-native.md#notes) has the details.

### WSL2 / Docker Desktop caveat

ROCm inside WSL2 works only for a handful of card/driver combinations. On a Windows AMD host,
run natively: the win-rocm binary talks to the driver directly (with the SDK above), and that
is what `golem up` installs by default there.

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
rocm and vulkan release assets and the `golem:rocm` image are pinned from upstream but have
never been through this repo's eval gate. On AMD hardware, right after the first `golem up`,
run it for each backend worth keeping:

```bash
golem eval --long 200000   # adopt only on PASS — same rule as a CUDA upgrade
```

## VRAM readings

The tuner, `golem bench`, `golem status` and the console read the card's VRAM without
vendor tools:

| | total | in use |
|---|---|---|
| NVIDIA | `nvidia-smi` | `nvidia-smi` |
| Windows, any vendor | the display class's registry key (`HardwareInformation.qwMemorySize`, or the binary `MemorySize` some AMD drivers write) | the `GPUAdapterMemory` performance counters, through WMI |
| Linux, amdgpu | `/sys/class/drm/card*/device/mem_info_vram_total` | `mem_info_vram_used` |

With several adapters the largest one is the card (a discrete Radeon beats a Ryzen iGPU's
carve-out). The Windows counters name adapters by LUID only, so the in-use figure is the
highest of them: on a box with an iGPU next to the card, an idle card can read the iGPU's
use until a model is loaded. A Windows reading costs one PowerShell start (~0.4 s).

Also expect the VRAM-headroom notes in [models.md](models.md) to shift: the silent
spill-to-shared-memory threshold that governs `--n-cpu-moe` tuning on the NVIDIA reference box
behaves differently (and not always silently) per AMD driver. Re-run `golem bench` after any
offload change.

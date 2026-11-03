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
| native (Linux, Windows) | the `linux_asset_rocm` / `windows_asset_rocm` release the adapter names — the official `llama-*-bin-ubuntu-rocm-*-x64.tar.gz` or `llama-*-bin-win-rocm-*-x64.zip`, installed by `golem up` or `golem install`. On Windows golem also installs the ROCm libraries that archive lacks, below |
| container | the `rocm` build arg: `docker build --build-arg GPU_BACKEND=rocm -t golem:rocm .`, run with `/dev/kfd` and `/dev/dri` passed through instead of `gpus: all` ([compose.yml](../compose.yml), `--profile rocm`) |

## Windows: golem installs the ROCm runtime

llama.cpp's Windows ROCm archive (`llama-b11379-bin-win-rocm-10.0-x64.zip`) carries the HIP
runtime - `amdhip64_7.dll`, `amd_comgr.dll`, `rocm_kpack.dll` - but not the BLAS libraries
its `ggml-hip.dll` imports. Without them HIP never initialises and the engine runs every
layer on the CPU without an error. AMD publishes those libraries for Windows in its ROCm
SDK wheels, and golem fetches them itself:

- When the backend is `rocm` and `hipblas.dll` or the rest are not beside
  `llama-server.exe` (and hipBLAS and rocBLAS are not on PATH, where an installed AMD
  HIP SDK puts them), `golem install` and `golem up` download
  `rocm_sdk_libraries_custom-7.2.1-py3-none-win_amd64.whl` (467 MB) from
  `repo.radeon.com`. ROCm 7.2.1 is the major the zip's `amdhip64_7.dll` is.
- The wheel is kept in `data/cache/` and refused unless its SHA-256 is the one the adapter
  pins (`windows_libraries_rocm_sha256` in [config/engines/llama.cpp.ini](../config/engines/llama.cpp.ini)).
- Only what the engine loads is extracted beside the server: `hipblas.dll`, `rocblas.dll`,
  `rocsolver.dll` and `libhipblaslt.dll` (what `hipblas.dll` imports, and what they import
  in turn), and the `rocblas\` and `hipblaslt\` kernel folders - 207 of their files are gfx1101 kernels.
  MIOpen, rocRAND and the rest of the wheel are left out. That is about 870 MB beside the
  engine.
- No admin rights and no PATH change: Windows looks for a DLL in the executable's folder
  first. A second `up` finds the files and does nothing; a new engine build extracts the
  kept wheel again rather than downloading it.

The install says what it does:

```
[ .. ] the rocm build lacks hipblas.dll, rocblas.dll, rocsolver.dll, libhipblaslt.dll, rocblas, hipblaslt
[ .. ] fetching rocm_sdk_libraries_custom-7.2.1-py3-none-win_amd64.whl (467 MB) from repo.radeon.com
[ .. ] extracting hipblas.dll, rocblas.dll, rocsolver.dll, libhipblaslt.dll, rocblas, hipblaslt from rocm_sdk_libraries_custom-7.2.1-py3-none-win_amd64.whl
[ OK ] placed hipblas.dll, rocblas.dll, rocsolver.dll, libhipblaslt.dll, rocblas, hipblaslt beside the engine in ...\b11379-rocm
```

**What is proven, and what is not.** On the reference box (NVIDIA) the download, the hash,
the extraction and the placement run as above, the "not found beside the engine" line goes
quiet, and every function `ggml-hip.dll` and the four libraries import resolves against the
zip's own `amdhip64_7.dll`, `hipblas.dll`, `rocblas.dll`, `rocsolver.dll` and
`libhipblaslt.dll`. Whether HIP then finds an AMD card, and how fast it is, is **unproven
until an RX 7800 XT runs it**: `--list-devices` listing `ROCm0` is the first check, and
`golem eval --long 200000` the one that decides.

### When the runtime cannot be fetched

With `GPU_BACKEND = "auto"` an AMD card falls back to **Vulkan**, which needs nothing
beyond the driver: `up` says why the fetch failed, writes the reason to
`.local/rocm-unavailable.txt`, installs the Vulkan build and serves on it. `auto` keeps
choosing Vulkan while that file is there; `golem install` removes it and tries ROCm again.
With `GPU_BACKEND = "rocm"` set by hand there is no fallback: `up` says the fetch failed,
starts the ROCm build, and the device probe below says it has no GPU.

## Vulkan

`GPU_BACKEND = "vulkan"` installs the same release's `llama-*-bin-win-vulkan-x64.zip` or
`llama-*-bin-ubuntu-vulkan-x64.tar.gz` (the adapter's `windows_asset_vulkan` /
`linux_asset_vulkan`). It needs nothing beyond the GPU driver and has no gfx target list, so
it is the fallback when the ROCm runtime cannot be fetched and the backend to try when the
ROCm build does not load on a card; which of the two is faster on a given card is for
`golem bench` to say. It runs on an NVIDIA card too, which is how the non-CUDA path —
install, start, bench, tune — is exercised on the NVIDIA reference box.

### The 35B under Vulkan

b11379 locks a model's host weights from the buffer's base address, and under Vulkan a host
buffer over 4 GiB - the 35B, with its experts in RAM - is split into chunks with no base:
`load-mode = mlock` aborts the load with `llama-mmap.cpp:804: GGML_ASSERT(addr) failed`
(exit code `-1073740791`). That is the same on the 3060 and the RX 7800 XT, so it is
Vulkan's, not AMD's. `mmap+mlock` pins the same weights through the file mapping and loads.
The adapter says so - `served.vulkan.load-mode.mlock = mmap+mlock` - and a Vulkan engine
reads the preset with that line replaced (`data/tuned/models.served.ini`), a tune's gate
included; the preset itself keeps `mlock`, which the CUDA build loads. `mmap+mlock` pins the
whole mapping (21.7 GB for the 35B), so it needs the RAM to hold it. Upstream locks each
chunk after b11379 (PR #30926), and the adapter's line goes when the build moves past it.
[windows-native.md](windows-native.md#notes) has the details.

## Requirements

- A [ROCm-supported](https://rocm.docs.amd.com/projects/install-on-linux/en/latest/reference/system-requirements.html)
  AMD GPU (RDNA2/RDNA3 discrete cards and most recent ones are fine) with an up-to-date
  driver. The release's ROCm build covers gfx1100, gfx1101 (RX 7800 XT), gfx1030 and the
  other official targets.
- **Windows, ROCm:** the Adrenalin driver. golem installs the rest, above; an AMD HIP SDK
  already on PATH is used as it is. No build tools are needed, because golem installs a
  release.
- **Windows, Vulkan:** nothing beyond the driver.
- Container on **Linux**: nothing beyond the driver — the container maps `/dev/kfd` (compute)
  and `/dev/dri` (display) itself.

### A build that found no GPU

Before each start golem asks the installed engine `llama-server --list-devices`. The
answer is kept for `golem status` (`device: ROCm0 AMD Radeon RX 7800 XT`) and the console's
status strip. When it lists no device, `up` says so in plain words - for the Windows ROCm
build, which of `hipblas.dll` and `rocblas.dll` is still missing beside the engine and on
PATH, or, with both there, that HIP found no card it supports - the console shows the model
as served "on the CPU only", and `bench`, `eval` and `tune` refuse to run, so no CPU number
is ever filed as the card's.

### WSL2 / Docker Desktop caveat

ROCm inside WSL2 works only for a handful of card/driver combinations. On a Windows AMD host,
run natively: the win-rocm binary talks to the driver directly, and that is what `golem up`
installs by default there.

## gfx targets

The pinned release is built for the official ROCm targets, so a supported card needs nothing
here. A build of your own — not something golem does, since `golem install` fetches the
pinned release rather than compiling — is where `AMDGPU_TARGETS=gfx1100` (RX 7900),
`gfx1030` (RX 6000) and the like come in. APU and lesser variants (gfx1102, gfx1032, ...)
aren't official targets; the usual workaround is `HSA_OVERRIDE_GFX_VERSION` (e.g. `11.0.2`
for gfx1102, `10.3.0` for gfx1032) exported in the environment of the engine — the service
passes its own environment to the engine it starts, so setting it before `golem service` is
what reaches the model. See AMD's ROCm docs.

## Status and the eval gate

**AMD itself is unproven.** The reference machine (NVIDIA RTX 3060 12 GB) runs the Vulkan
build - the 35B's load, `bench` and `eval --long 200000` under Vulkan are measured there -
and the ROCm install up to the libraries beside the engine. Neither backend has been through
this repo's eval gate on an AMD card. On AMD hardware, right after the first `golem up`,
run it for each backend worth keeping:

```bash
golem eval --long 200000   # adopt only on PASS — same rule as a CUDA upgrade
```

Its result on both backends decides which one `auto` gives AMD.

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

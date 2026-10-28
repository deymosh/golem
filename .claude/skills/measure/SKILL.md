---
name: measure
description: Measure a model, an engine build or a flag on the reference box the way this repo decides things - one engine-touching run at a time, in the background, paired, gated. Use before any bench, eval, tune, build trial or model comparison.
---

# Measuring in golem

Every decision here comes from a measurement on real hardware, and a measurement
is only worth something when it was taken the same way as the one it is compared
with. This is that way.

## Before anything touches the GPU

1. The GPU is someone's desktop. Ask first unless the user has said the machine is
   free for this run.
2. Nothing else may be measuring: `tasklist | findstr /i "golem llama-server"`
   (or `Get-CimInstance Win32_Process`) shows a running `bench`, `eval` or `tune`.
   Never build while one runs - the build fights it for CPU and RAM.
3. Free RAM: a 35B MoE pins ~13 GB. `(Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory/1MB`.
   Docker Desktop's WSL VM can hold 10 GB of page cache; drop it with
   `wsl -d docker-desktop -u root -e sh -c "sync; echo 3 > /proc/sys/vm/drop_caches"`.
   Never terminate a WSL distro (an unattended upgrade may be running in it).
4. `golem service` must already be running, started interactively
   (`Start-Process .\golem.exe service -WindowStyle Hidden`). A background script
   that starts it hands the service its pipe and never sees EOF.
5. Back up what the trial edits: `golem.toml` and `data/tuned/models.effective.ini`
   into `.local/backup/<date>/`.

## Running

- Long runs go to `logs/<name>.log` from a background shell, with `RECORD_NOTE`
  saying what the row is (`RECORD_NOTE="pair 2: b11936 mmap+mlock, <date>" ./golem.exe bench`).
- One engine-touching command at a time: `up`, `bench`, `eval`, `tune`, `restart`.
- Wait with a background `until grep -q "^exit=" logs/x.log; do sleep 20; done`,
  not a polling loop in the foreground.
- A failed load: read `logs/llama-server.log` and `.local/load-failure.txt`. A CUDA
  out of memory on load resets the display driver on Windows (a black screen for a
  moment); the service stops the engine after one failed load.

## Comparing

- Pairs in the same session, alternating (A B B A), on the same preset; best-of-3
  is what `bench` already does inside one run.
- Same size class only; a rate identifies its model - sanity-check the numbers
  against the model before trusting a verdict.
- Decode at depth and short decode decide for a coder; prefill alone never does.

## Trying an engine build

1. `ENGINE_BUILD = "b<N>"` in golem.toml, then `golem install`.
2. Since b11879, `load-mode = mlock` fails on load (CUDA OOM in
   `ggml_backend_cuda_buffer_set_tensor`); `load-mode = mmap+mlock` loads and pins
   the same way. Trial newer builds with `mmap+mlock` in the served section.
3. `golem up`, `bench`, `eval --long 200000`, then the pairs against the pinned
   build. The adapter's `build` moves only on a better gated verdict.
4. Restore golem.toml and the effective preset from the backup.

## Adopting

Nothing - model, build, flag - becomes a default without `golem eval --long 200000`
passing on this machine. `golem tune <preset> --adopt` gates before it writes; record
what was measured, and why, in the commit body and in docs/models.md or
docs/engines-and-models.md.

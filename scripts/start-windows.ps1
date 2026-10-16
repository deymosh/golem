# Golem - native llama-server on Windows (no Docker, no WSL).
#
# Reads the same .env as every other script and mirrors the compose.yml
# flags exactly, so a model configured for docker mode behaves identically
# here. On first run it downloads the pinned official llama.cpp CUDA or
# ROCm build (GPU_BACKEND in .env; auto = detect the adapter, LLAMA_WIN_BUILD
# in .env, default below) from GitHub releases into .local\bin-windows.
# Builds are gated like the docker image digest: adopt a new one only after
# `eval-model.sh --long 200000` passes on it.
#
# Usage:  powershell -ExecutionPolicy Bypass -File scripts\start-windows.ps1
# Stop:   powershell -ExecutionPolicy Bypass -File scripts\stop-windows.ps1
#
# LAN exposure: bind 0.0.0.0 (LOCAL_AI_HOST) and open the port once, as admin:
#   New-NetFirewallRule -DisplayName "Golem inference" -Direction Inbound `
#     -LocalPort 8000 -Protocol TCP -Action Allow
#
# Why native: direct CUDA/ROCm instead of WSL's paravirtualized GPU (~+50%
# batch-1 decode on the reference machine), no vmmem monolith (page cache
# becomes trimmable standby), no Docker Desktop overhead, no VM that can
# OOM-kill the stack. See docs/windows-native.md.

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot

# ------------------------------------------------------------
# .env (single config source, same as docker mode)
# ------------------------------------------------------------

$Cfg = @{}
Get-Content (Join-Path $Root ".env") | ForEach-Object {
    if ($_ -match '^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*?)\s*$') {
        $v = $Matches[2]
        if ($v.Length -ge 2 -and $v.StartsWith('"') -and $v.EndsWith('"')) {
            $v = $v.Substring(1, $v.Length - 2)
        }
        $Cfg[$Matches[1]] = $v
    }
}
if ($Cfg.Count -eq 0) { throw "No usable .env at $Root\.env (copy .env.example first)" }

$BindHost  = if ($Cfg["LOCAL_AI_HOST"])     { $Cfg["LOCAL_AI_HOST"] }     else { "127.0.0.1" }
$Port      = if ($Cfg["LOCAL_AI_PORT"])     { $Cfg["LOCAL_AI_PORT"] }     else { "8000" }
$ApiKey    = if ($Cfg["LOCAL_AI_API_KEY"])  { $Cfg["LOCAL_AI_API_KEY"] }  else { "local-key" }
$ModelFile = if ($Cfg["MODEL_FILE"])        { $Cfg["MODEL_FILE"] }        else { "Qwen3.6-35B-A3B-UD-Q4_K_M.gguf" }
$Alias     = if ($Cfg["MODEL_ALIAS"])       { $Cfg["MODEL_ALIAS"] }       else { "local-model" }
$Ctx       = if ($Cfg["CONTEXT_SIZE"])      { $Cfg["CONTEXT_SIZE"] }      else { "32768" }
$Threads   = if ($Cfg["CPU_THREADS"])       { $Cfg["CPU_THREADS"] }       else { "8" }
$Parallel  = if ($Cfg["PARALLEL"])          { $Cfg["PARALLEL"] }          else { "1" }
$GpuLayers = if ($Cfg["GPU_LAYERS"])        { $Cfg["GPU_LAYERS"] }        else { "99" }
$CacheK    = if ($Cfg["CACHE_TYPE_K"])      { $Cfg["CACHE_TYPE_K"] }      else { "q8_0" }
$CacheV    = if ($Cfg["CACHE_TYPE_V"])      { $Cfg["CACHE_TYPE_V"] }      else { "q8_0" }
$LoadMode  = if ($Cfg["LOAD_MODE"])         { $Cfg["LOAD_MODE"] }         else { "mlock" }
$SpecType  = $Cfg["SPEC_TYPE"]
$DraftFile = $Cfg["DRAFT_FILE"]
$ExtraArgs = $Cfg["EXTRA_ARGS"]
$SleepIdle = $Cfg["SLEEP_IDLE_SECONDS"]
$Preset    = $Cfg["MODELS_PRESET"]

# GPU vendor: auto = read it off the display adapters; GPU_BACKEND in .env
# overrides for headless/remote/wrong-guess boxes.
$Backend = if ($Cfg["GPU_BACKEND"]) { $Cfg["GPU_BACKEND"] } else { "auto" }
if ($Backend -eq "auto") {
    # CIM first (PS 5.1+ has it); AdapterCompatibility is the vendor string.
    $adapters = @(Get-CimInstance Win32_VideoController -ErrorAction SilentlyContinue)
    if (-not $adapters) { $adapters = @(Get-WmiObject Win32_VideoController -ErrorAction SilentlyContinue) }
    $vendors = ($adapters | ForEach-Object { "$($_.AdapterCompatibility) $($_.Name)" }) -join ";"
    if     ($vendors -match "NVIDIA")          { $Backend = "cuda" }
    elseif ($vendors -match "AMD|ATI")         { $Backend = "rocm" }
    else { throw "Could not detect the GPU vendor (adapters: '$vendors'). Set GPU_BACKEND=cuda or rocm in .env (see docs/amd-rocm.md)" }
}
if ($Backend -ne "cuda" -and $Backend -ne "rocm") { throw "GPU_BACKEND must be auto, cuda or rocm (got: $Backend)" }

$ModelDir = Join-Path $Root "data\models"
if ($Preset) {
    # Router mode: models and their flags come from the preset file.
    $PresetPath = Join-Path $Root $Preset
    if (-not (Test-Path $PresetPath)) { throw "Preset not found: $PresetPath (MODELS_PRESET in .env)" }
} else {
    $ModelPath = Join-Path $ModelDir $ModelFile
    if (-not (Test-Path $ModelPath)) { throw "Model not found: $ModelPath (./golem pull downloads it)" }
}

# ------------------------------------------------------------
# Port conflict check (docker inference or a previous instance)
# ------------------------------------------------------------

if (Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue) {
    throw "Port $Port is already in use. Stop the other server first:" +
          " docker -> './golem down', native -> scripts\stop-windows.ps1"
}

# ------------------------------------------------------------
# llama-server (official CUDA/ROCm build, downloaded once)
# ------------------------------------------------------------

$BinDir = Join-Path $Root ".local\bin-windows"
$Server = Join-Path $BinDir "llama-server.exe"
$BuildMarker = Join-Path $BinDir "BUILD"

# Known-good build: passed the 200K needle + tool calls on this platform.
$WinBuild = if ($Cfg["LLAMA_WIN_BUILD"]) { $Cfg["LLAMA_WIN_BUILD"] } else { "b11379" }

# The marker stores "<tag> <backend>": switching GPU_BACKEND must not reuse
# the other vendor's exe, so a backend mismatch wipes like a build mismatch.
# Pre-backend markers ("<tag>") mismatch too — a one-time reinstall that
# upgrades the marker, never a silent wrong-vendor run.
$installed = if (Test-Path $BuildMarker) { (Get-Content $BuildMarker -Raw).Trim() } else { "<no build marker>" }
# A missing marker counts as a mismatch: a killed install can leave the exe
# extracted without the marker, and that half-state must reinstall, not run.
if ((Test-Path $Server) -and $installed -ne "$WinBuild $Backend") {
    Write-Host "[ .. ] installed llama.cpp $installed != pinned $WinBuild ($Backend), reinstalling..."
    Remove-Item -Recurse -Force $BinDir
}

if (-not (Test-Path $Server)) {
    Write-Host "[ .. ] llama-server not found, downloading official $Backend build $WinBuild..."
    New-Item -ItemType Directory -Force $BinDir | Out-Null

    # "latest" follows upstream (ungated); any b<number> tag pins that build.
    if ($WinBuild -eq "latest") {
        # Skip stub releases (e.g. v0.5.0, a nightly pointer with no assets):
        # real builds are tagged b<number> and carry the win-cuda/rocm zips.
        $rels = Invoke-RestMethod "https://api.github.com/repos/ggml-org/llama.cpp/releases?per_page=15"
        $rel = $rels | Where-Object { $_.tag_name -match "^b\d+$" -and @($_.assets).Count -gt 0 } | Select-Object -First 1
    } else {
        $rel = Invoke-RestMethod "https://api.github.com/repos/ggml-org/llama.cpp/releases/tags/$WinBuild"
    }
    if (-not $rel) { throw "No binary llama.cpp release found on GitHub" }
    $tag = $rel.tag_name

    # CUDA ships a separate cudart zip (runtime DLLs); the rocm build bundles
    # its HIP runtime in the main zip.
    if ($Backend -eq "cuda") {
        $serverZip = $rel.assets | Where-Object { $_.name -match "^llama-.*bin-win-cuda-13\.\d+-x64\.zip$" } | Select-Object -First 1
        $cudartZip = $rel.assets | Where-Object { $_.name -match "^cudart-llama-bin-win-cuda-13\.\d+-x64\.zip$" } | Select-Object -First 1
        if (-not $serverZip) { throw "No win-cuda x64 asset found in release $tag" }
        $assets = @($serverZip) + @($cudartZip | Where-Object { $_ })
    } else {
        $serverZip = $rel.assets | Where-Object { $_.name -match "^llama-.*bin-win-rocm-\d+\.\d+-x64\.zip$" } | Select-Object -First 1
        if (-not $serverZip) { throw "No win-rocm x64 asset found in release $tag" }
        $assets = @($serverZip)
    }

    $tmp = Join-Path $env:TEMP "llama-cpp-$tag"
    foreach ($asset in $assets) {
        Write-Host "[ .. ] $($asset.name) ($([math]::Round($asset.size/1MB)) MB)"
        $zip = Join-Path $tmp $asset.name
        New-Item -ItemType Directory -Force $tmp | Out-Null
        Invoke-WebRequest $asset.browser_download_url -OutFile $zip
        Expand-Archive $zip $BinDir -Force
    }
    Set-Content -Path $BuildMarker -Value "$tag $Backend"
    Write-Host "[ OK ] llama.cpp $tag ($Backend) installed to $BinDir"
}

# ------------------------------------------------------------
# Launch (flags mirror compose.yml)
# ------------------------------------------------------------

$LogDir = Join-Path $Root "logs"
New-Item -ItemType Directory -Force $LogDir | Out-Null
$Log = Join-Path $LogDir "llama-server-windows.log"

$ServerArgs = @(
    "--host", $BindHost,
    "--port", $Port,
    "--threads", $Threads,
    "--parallel", $Parallel,
    "--metrics",
    "--api-key", $ApiKey,
    "--log-file", $Log,
    "--log-colors", "off"
)
if ($SleepIdle) { $ServerArgs += @("--sleep-idle-seconds", $SleepIdle) }
if ($Preset) {
    # Router: command-line flags override every preset, so only the shared
    # ones above go here; one model resident at a time (12 GB card).
    $ServerArgs += @("--models-preset", $PresetPath, "--models-max", "1")
} else {
    $ServerArgs += @(
        "--model", $ModelPath,
        "--alias", $Alias,
        "--ctx-size", $Ctx,
        "--n-gpu-layers", $GpuLayers,
        "--jinja",
        "-fa", "on",
        "--cache-type-k", $CacheK,
        "--cache-type-v", $CacheV,
        "--load-mode", $LoadMode
    )
    if ($SpecType) { $ServerArgs += @("--spec-type", $SpecType) }
    if ($DraftFile) { $ServerArgs += @("--spec-draft-model", (Join-Path $ModelDir $DraftFile)) }
    if ($ExtraArgs) { $ServerArgs += ($ExtraArgs -split '\s+' | Where-Object { $_ }) }
}
# Start-Process joins arguments with bare spaces: quote paths that have them.
$ServerArgs = $ServerArgs | ForEach-Object { if ("$_" -match '\s') { "`"$_`"" } else { "$_" } }

# The server writes its own log (--log-file) instead of -Redirect*: those
# make Start-Process spawn with handle inheritance, so llama-server would
# keep the caller's stdout pipe open and hang anything reading it until
# EOF (e.g. `./golem up | tail`).
# Started from data\models so preset model paths resolve as bare filenames.
$Proc = Start-Process -FilePath $Server -ArgumentList $ServerArgs -WorkingDirectory $ModelDir -WindowStyle Hidden -PassThru
New-Item -ItemType Directory -Force (Join-Path $Root ".local") | Out-Null
Set-Content -Path (Join-Path $Root ".local\llama-server-windows.pid") -Value $Proc.Id
Write-Host "[ OK ] llama-server started, PID $($Proc.Id)"
Write-Host "       logs: $Log"

# ------------------------------------------------------------
# Wait for health
# ------------------------------------------------------------

Write-Host "[ .. ] Waiting for the model to load (first boot includes GPU warmup)..."
$deadline = (Get-Date).AddMinutes(10)
$healthy = $false
while ((Get-Date) -lt $deadline) {
    if ($Proc.HasExited) {
        if (Test-Path $Log) { Get-Content $Log -Tail 15 }
        throw "llama-server exited during startup (exit code $($Proc.ExitCode))"
    }
    # curl.exe, not Invoke-WebRequest: PS 5.1 honors the system proxy,
    # which breaks/loops requests to localhost on proxy-rigged machines.
    # -fs without -S: no stderr at all (503 while loading, refused before
    # listening), since PS 5.1 turns redirected native stderr into a
    # terminating error under ErrorActionPreference=Stop.
    curl.exe -fs --noproxy '*' -m 2 -o NUL "http://127.0.0.1:$Port/health"
    if ($LASTEXITCODE -eq 0) { $healthy = $true; break }
    Start-Sleep -Seconds 3
}
if (-not $healthy) { throw "Server did not become healthy within 10 minutes (logs: $Log)" }

if ($Preset) {
    Write-Host "[ OK ] router healthy on http://$BindHost`:$Port (presets: $Preset; models load on first request)"
} else {
    Write-Host "[ OK ] healthy on http://$BindHost`:$Port (model: $ModelFile)"
}
Write-Host ""
Write-Host "Point Claude Code at Golem:"
Write-Host "`$env:ANTHROPIC_BASE_URL = `"http://127.0.0.1:$Port`""
Write-Host "`$env:ANTHROPIC_AUTH_TOKEN = `"$ApiKey`""
Write-Host "`$env:ANTHROPIC_MODEL = `"$Alias`""
Write-Host "`$env:ANTHROPIC_SMALL_FAST_MODEL = `"$Alias`""

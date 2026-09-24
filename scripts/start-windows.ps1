# Golem - native llama-server on Windows (no Docker, no WSL).
#
# Reads the same .env as every other script and mirrors the compose.yml
# flags exactly, so a model configured for docker mode behaves identically
# here. On first run it downloads the official llama.cpp CUDA build from
# GitHub releases into .local\bin-windows.
#
# Usage:  powershell -ExecutionPolicy Bypass -File scripts\start-windows.ps1
# Stop:   powershell -ExecutionPolicy Bypass -File scripts\stop-windows.ps1
#
# LAN exposure: bind 0.0.0.0 (LOCAL_AI_HOST) and open the port once, as admin:
#   New-NetFirewallRule -DisplayName "Golem inference" -Direction Inbound `
#     -LocalPort 8000 -Protocol TCP -Action Allow
#
# Why native: no WSL vmmem monolith (page cache becomes trimmable standby),
# no Docker Desktop overhead (~1 GB), no VM that can OOM-kill the stack, and
# direct CUDA. Expect a few percent more speed; stability is the point.
# See docs/windows-native.md.

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

$ModelPath = Join-Path $Root "data\models\$ModelFile"
if (-not (Test-Path $ModelPath)) { throw "Model not found: $ModelPath (./golem pull downloads it)" }

# ------------------------------------------------------------
# Port conflict check (docker inference or a previous instance)
# ------------------------------------------------------------

if (Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue) {
    throw "Port $Port is already in use. Stop the other server first:" +
          " docker -> './golem down', native -> scripts\stop-windows.ps1"
}

# ------------------------------------------------------------
# llama-server (official CUDA build, downloaded once)
# ------------------------------------------------------------

$BinDir = Join-Path $Root ".local\bin-windows"
$Server = Join-Path $BinDir "llama-server.exe"

if (-not (Test-Path $Server)) {
    Write-Host "[ .. ] llama-server not found, downloading official CUDA build..."
    New-Item -ItemType Directory -Force $BinDir | Out-Null

    # Skip stub releases (e.g. v0.5.0, a nightly pointer with no assets):
    # real builds are tagged b<number> and carry the win-cuda zips.
    $rels = Invoke-RestMethod "https://api.github.com/repos/ggml-org/llama.cpp/releases?per_page=15"
    $rel = $rels | Where-Object { $_.tag_name -match "^b\d+$" -and @($_.assets).Count -gt 0 } | Select-Object -First 1
    if (-not $rel) { throw "No binary llama.cpp release found on GitHub" }
    $tag = $rel.tag_name
    $serverZip = $rel.assets | Where-Object { $_.name -match "^llama-.*bin-win-cuda-13\.\d+-x64\.zip$" } | Select-Object -First 1
    $cudartZip = $rel.assets | Where-Object { $_.name -match "^cudart-llama-bin-win-cuda-13\.\d+-x64\.zip$" } | Select-Object -First 1
    if (-not $serverZip) { throw "No win-cuda x64 asset found in release $tag" }

    $tmp = Join-Path $env:TEMP "llama-cpp-$tag"
    foreach ($asset in @($serverZip) + @($cudartZip | Where-Object { $_ })) {
        Write-Host "[ .. ] $($asset.name) ($([math]::Round($asset.size/1MB)) MB)"
        $zip = Join-Path $tmp $asset.name
        New-Item -ItemType Directory -Force $tmp | Out-Null
        Invoke-WebRequest $asset.browser_download_url -OutFile $zip
        Expand-Archive $zip $BinDir -Force
    }
    Write-Host "[ OK ] llama.cpp $tag installed to $BinDir"
}

# ------------------------------------------------------------
# Launch (flags mirror compose.yml)
# ------------------------------------------------------------

$LogDir = Join-Path $Root "logs"
New-Item -ItemType Directory -Force $LogDir | Out-Null
$OutLog = Join-Path $LogDir "llama-server-windows.out.log"
$ErrLog = Join-Path $LogDir "llama-server-windows.err.log"

$ServerArgs = @(
    "--host", $BindHost,
    "--port", $Port,
    "--model", $ModelPath,
    "--alias", $Alias,
    "--ctx-size", $Ctx,
    "--n-gpu-layers", $GpuLayers,
    "--threads", $Threads,
    "--parallel", $Parallel,
    "--jinja",
    "--metrics",
    "-fa", "on",
    "--cache-type-k", $CacheK,
    "--cache-type-v", $CacheV,
    "--load-mode", $LoadMode,
    "--api-key", $ApiKey
)
if ($SpecType) { $ServerArgs += @("--spec-type", $SpecType) }
if ($DraftFile) { $ServerArgs += @("--spec-draft-model", (Join-Path $Root "data\models\$DraftFile")) }
if ($ExtraArgs) { $ServerArgs += ($ExtraArgs -split '\s+') }

$Proc = Start-Process -FilePath $Server -ArgumentList $ServerArgs `
    -RedirectStandardOutput $OutLog -RedirectStandardError $ErrLog `
    -WindowStyle Hidden -PassThru
New-Item -ItemType Directory -Force (Join-Path $Root ".local") | Out-Null
Set-Content -Path (Join-Path $Root ".local\llama-server-windows.pid") -Value $Proc.Id
Write-Host "[ OK ] llama-server started, PID $($Proc.Id)"
Write-Host "       logs: $ErrLog"

# ------------------------------------------------------------
# Wait for health
# ------------------------------------------------------------

Write-Host "[ .. ] Waiting for the model to load (first boot includes CUDA warmup)..."
$deadline = (Get-Date).AddMinutes(10)
$healthy = $false
while ((Get-Date) -lt $deadline) {
    if ($Proc.HasExited) {
        Get-Content $ErrLog -Tail 15
        throw "llama-server exited during startup (exit code $($Proc.ExitCode))"
    }
    # curl.exe, not Invoke-WebRequest: PS 5.1 honors the system proxy,
    # which breaks/loops requests to localhost on proxy-rigged machines.
    if (curl.exe -fsS --noproxy '*' -m 2 "http://127.0.0.1:$Port/health" 2>$null) {
        $healthy = $true; break
    }
    Start-Sleep -Seconds 3
}
if (-not $healthy) { throw "Server did not become healthy within 10 minutes (logs: $ErrLog)" }

Write-Host "[ OK ] healthy on http://$BindHost`:$Port (model: $ModelFile)"
Write-Host ""
Write-Host "Point Claude Code at Golem:"
Write-Host "`$env:ANTHROPIC_BASE_URL = `"http://127.0.0.1:$Port`""
Write-Host "`$env:ANTHROPIC_AUTH_TOKEN = `"$ApiKey`""
Write-Host "`$env:ANTHROPIC_MODEL = `"$Alias`""
Write-Host "`$env:ANTHROPIC_SMALL_FAST_MODEL = `"$Alias`""

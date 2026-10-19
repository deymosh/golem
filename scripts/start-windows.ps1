# Golem - native llama-server on Windows (no Docker, no WSL).
#
# Reads the same .env as every other script and mirrors the compose.yml
# flags exactly, so a model configured for docker mode behaves identically
# here. On first run it downloads the pinned CUDA or ROCm build of the
# engine named by ENGINE in .env (default llama.cpp; release source and pin
# in engines\<name>\engine.ini; GPU_BACKEND auto-detects the adapter) from
# GitHub releases into .local\engines\<name>\<build>-<backend>.
# Builds are gated like the docker image digest: adopt a new one only after
# `eval-model.sh --long 200000` passes on it.
#
# Usage:  powershell -ExecutionPolicy Bypass -File scripts\start-windows.ps1 [-DryRun]
#         -DryRun: install what is missing, print the server command line, exit
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

param([switch]$DryRun)

$ErrorActionPreference = "Stop"
# PS 5.1 redraws Invoke-WebRequest's progress bar per chunk, which slows a
# 1 GB engine download to a crawl.
$ProgressPreference = "SilentlyContinue"
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
$ModelRepo = if ($Cfg["HF_REPO"])           { $Cfg["HF_REPO"] }           else { "unsloth/Qwen3.6-35B-A3B-GGUF" }
$DraftRepo = if ($Cfg["DRAFT_HF_REPO"])     { $Cfg["DRAFT_HF_REPO"] }     else { $ModelRepo }
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
}

# ------------------------------------------------------------
# Port conflict check (docker inference or a previous instance)
# ------------------------------------------------------------

if (Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue) {
    throw "Port $Port is already in use. Stop the other server first:" +
          " docker -> './golem down', native -> scripts\stop-windows.ps1"
}

# ------------------------------------------------------------
# Engine (ENGINE in .env -> engines\<name>\engine.ini; pinned build
# downloaded once into .local\engines\<name>\<build>-<backend>)
# ------------------------------------------------------------

$EngineName = if ($Cfg["ENGINE"]) { $Cfg["ENGINE"] } else { "llama.cpp" }
$EngineIni = Join-Path $Root "engines\$EngineName\engine.ini"
if (-not (Test-Path $EngineIni)) { throw "Unknown ENGINE '$EngineName' (no $EngineIni)" }
$Eng = @{}
Get-Content $EngineIni | ForEach-Object {
    if ($_ -match '^\s*([A-Za-z0-9_.\-]+)\s*=\s*(.*?)\s*$') { $Eng[$Matches[1]] = $Matches[2] }
}
$ServerExe = if ($Eng["server"]) { $Eng["server"] } else { "llama-server.exe" }

# Known-good build: passed the 200K needle + tool calls on this platform.
# LLAMA_WIN_BUILD is the pre-adapter name, still honored for llama.cpp.
$WinBuild = if ($Cfg["ENGINE_BUILD"]) { $Cfg["ENGINE_BUILD"] }
            elseif ($EngineName -eq "llama.cpp" -and $Cfg["LLAMA_WIN_BUILD"]) { $Cfg["LLAMA_WIN_BUILD"] }
            else { $Eng["build"] }
if (-not $WinBuild) { throw "No build pinned for engine $EngineName (build in $EngineIni or ENGINE_BUILD in .env)" }
$EngineRoot = Join-Path $Root ".local\engines\$EngineName"

if ($Eng["launcher"] -eq "strata") {
    # Strata installs itself with its own setup (private .venv, ready-made
    # engine, model files - everything under $EngineRoot\data). No GitHub
    # zip asset to fetch: a shallow clone of the tag, then setup.py
    # headless. Strata's engine is one build for NVIDIA and AMD, so the
    # build dir carries no backend suffix.
    if ($WinBuild -eq "latest") {
        $rels = Invoke-RestMethod "https://api.github.com/repos/$($Eng["source_repo"])/releases?per_page=10"
        $WinBuild = ($rels | Where-Object { $_.tag_name -match "^v\d+\.\d+\.\d+$" } | Select-Object -First 1).tag_name
        if (-not $WinBuild) { throw "No Strata release found on GitHub" }
    }
    $BinDir = Join-Path $EngineRoot $WinBuild
    $BuildMarker = Join-Path $BinDir ".golem-build"
    $Server = Join-Path $BinDir ".venv\Scripts\python.exe"   # the API server is Python
    $installed = if (Test-Path $BuildMarker) { (Get-Content $BuildMarker -Raw).Trim() } else { "" }
    if ($installed -ne $WinBuild) {
        if (Test-Path $BinDir) { Remove-Item -Recurse -Force $BinDir }
        New-Item -ItemType Directory -Force $EngineRoot | Out-Null
        Write-Host "[ .. ] installing Strata $WinBuild from github.com/$($Eng["source_repo"]) (its setup also fetches the engine binary and the model files)..."
        & git clone -q --depth 1 -b $WinBuild "https://github.com/$($Eng["source_repo"]).git" $BinDir
        if ($LASTEXITCODE -ne 0) { throw "git clone of Strata $WinBuild failed (is git on PATH?)" }
        if (-not (Test-Path $Server)) {
            # Same bootstrap as its START-HERE.bat: a venv from the py
            # launcher or python (3.10+, 64-bit); never the system install.
            $basePy = @((Get-Command py -ErrorAction SilentlyContinue),
                        (Get-Command python -ErrorAction SilentlyContinue)) |
                      Where-Object { $_ } | Select-Object -First 1
            if (-not $basePy) { throw "Strata needs Python 3.10+ (py launcher or python on PATH)" }
            & $basePy.Source -m venv (Join-Path $BinDir ".venv")
            if ($LASTEXITCODE -ne 0) { throw "Could not create Strata's .venv" }
        }
        # Strata 0.1.45 setup runs the engine to probe which model families
        # it supports, but puts only HIP dirs on PATH: its pip-installed
        # CUDA DLLs are not found and the engine dies with 0xC0000135.
        # Prepend them ourselves.
        $env:PATH = "$BinDir\.venv\Lib\site-packages\nvidia\cu13\bin\x86_64;$env:PATH"
        Push-Location $BinDir
        & $Server setup.py --yes --no-start --no-browser `
            --family $Eng["strata_family"] --model $Eng["strata_model"] `
            --data-dir (Join-Path $EngineRoot "data") --context $Eng["strata_context"] `
            --vision no --port $Port --host $BindHost --api-key $ApiKey `
            --vram-reserve-mib $Eng["strata_vram_reserve"]
        $code = $LASTEXITCODE
        Pop-Location
        if ($code -ne 0) { throw "Strata setup failed (exit $code); everything already done is kept - fix and rerun" }

        # Strata's setup sizes --expert-vram as vram - 3.0 - reserve, which
        # assumes weights + context + buffers are ~3 GB. On a 35B-A3B with a
        # long context they are ~7 GB, so the engine refuses to start
        # ("not enough VRAM left for the hybrid experts' buffers"). Leave the
        # flag out (the engine then fits experts to the VRAM free at load),
        # or set strata_expert_vram in the engine.ini to pin a budget.
        $cfgPath = (Get-ChildItem $BinDir -Filter "strata-*.json" |
                    Where-Object { $_.Name -notmatch "^strata-vision" } | Select-Object -First 1).FullName
        $cfg = Get-Content $cfgPath -Raw | ConvertFrom-Json
        $kept = @(); for ($i = 0; $i -lt $cfg.args.Count; $i++) {
            if ($cfg.args[$i] -eq "--expert-vram") { $i++; continue }
            $kept += $cfg.args[$i]
        }
        if ($Eng["strata_expert_vram"]) { $kept += @("--expert-vram", $Eng["strata_expert_vram"]) }
        $cfg.args = $kept
        $cfg | ConvertTo-Json -Depth 6 | Set-Content $cfgPath

        Set-Content -Path $BuildMarker -Value $WinBuild
        Write-Host "[ OK ] Strata $WinBuild installed to $BinDir"
    }
} else {
    $Repo = $Eng["windows_repo"]
    if (-not $Repo) { throw "Engine $EngineName has no Windows release source (windows_repo in $EngineIni)" }

    # "latest" follows upstream (ungated), so the tag is only known after asking.
    $Release = $null
    if ($WinBuild -eq "latest") {
        # Skip stub releases (e.g. a nightly pointer with no assets).
        $rels = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases?per_page=15"
        $Release = $rels | Where-Object { @($_.assets | Where-Object { $_.name -match $Eng["windows_asset_$Backend"] }).Count -gt 0 } | Select-Object -First 1
        if (-not $Release) { throw "No $Backend Windows release of $EngineName found on GitHub" }
        $WinBuild = $Release.tag_name
    }

    $EngineRoot = Join-Path $Root ".local\engines\$EngineName"
    $BinDir = Join-Path $EngineRoot "$WinBuild-$Backend"
    $Server = Join-Path $BinDir $ServerExe
    $BuildMarker = Join-Path $BinDir "BUILD"

    # One-time move of the pre-adapter install (.local\bin-windows) when it is
    # exactly this build: saves a ~300 MB download.
    $Legacy = Join-Path $Root ".local\bin-windows"
    if ($EngineName -eq "llama.cpp" -and -not (Test-Path $BinDir) -and (Test-Path (Join-Path $Legacy "BUILD"))) {
        if ((Get-Content (Join-Path $Legacy "BUILD") -Raw).Trim() -eq "$WinBuild $Backend") {
            New-Item -ItemType Directory -Force $EngineRoot | Out-Null
            Move-Item $Legacy $BinDir
            Write-Host "[ OK ] moved llama.cpp $WinBuild from .local\bin-windows to $BinDir"
        }
    }

    # The marker ("<tag> <backend>") is written only after every zip extracted:
    # a killed install leaves the exe without it, and that half-state must
    # reinstall, not run.
    $installed = if (Test-Path $BuildMarker) { (Get-Content $BuildMarker -Raw).Trim() } else { "" }
    if ($installed -ne "$WinBuild $Backend") {
        if (Test-Path $BinDir) { Remove-Item -Recurse -Force $BinDir }
        Write-Host "[ .. ] installing $EngineName $WinBuild ($Backend) from github.com/$Repo..."
        if (-not $Release) { $Release = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/tags/$WinBuild" }

        $serverZip = $Release.assets | Where-Object { $_.name -match $Eng["windows_asset_$Backend"] } | Select-Object -First 1
        if (-not $Eng["windows_asset_$Backend"] -or -not $serverZip) { throw "No $Backend Windows asset of $EngineName in release $WinBuild" }
        $assets = @($serverZip)
        # e.g. llama.cpp's CUDA build ships its runtime DLLs as a separate zip.
        if ($Eng["windows_runtime_$Backend"]) {
            $assets += @($Release.assets | Where-Object { $_.name -match $Eng["windows_runtime_$Backend"] } | Select-Object -First 1)
        }

        New-Item -ItemType Directory -Force $BinDir | Out-Null
        $tmp = Join-Path $env:TEMP "golem-$EngineName-$WinBuild"
        New-Item -ItemType Directory -Force $tmp | Out-Null
        foreach ($asset in $assets) {
            Write-Host "[ .. ] $($asset.name) ($([math]::Round($asset.size/1MB)) MB)"
            $zip = Join-Path $tmp $asset.name
            Invoke-WebRequest $asset.browser_download_url -OutFile $zip
            Expand-Archive $zip $BinDir -Force
        }
        # Some zips wrap everything in a top folder: find the server wherever it landed.
        if (-not (Test-Path $Server)) {
            $found = Get-ChildItem $BinDir -Recurse -Filter $ServerExe | Select-Object -First 1
            if (-not $found) { throw "$ServerExe not found in the $EngineName $WinBuild zips" }
            Get-ChildItem $found.DirectoryName | Move-Item -Destination $BinDir -Force
        }
        Set-Content -Path $BuildMarker -Value "$WinBuild $Backend"
        Write-Host "[ OK ] $EngineName $WinBuild ($Backend) installed to $BinDir"
    }
}

# ------------------------------------------------------------
# Models (downloaded on first run - a fresh clone must just work)
# ------------------------------------------------------------

# curl.exe against HF's resolve endpoint, mirroring download-model.sh:
# -L follows the CDN redirect, -C - resumes a leftover .part, --retry
# rides out flaky links. Silent (-s): curl's progress meter writes to
# stderr, which PS 5.1 turns into a terminating error under
# ErrorActionPreference=Stop; the size HEAD tells the user what's coming.
function Ensure-Model([string]$Repo, [string]$File) {
    $path = Join-Path $ModelDir $File
    if ((Test-Path $path) -and (Get-Item $path).Length -gt 1GB) { return }
    if (Test-Path $path) {
        Write-Host "[ .. ] $File is smaller than 1 GB (partial download), retrying"
        Remove-Item $path -Force
    }

    $auth = @()
    if ($Cfg["HF_TOKEN"]) { $auth = @("-H", "Authorization: Bearer $($Cfg['HF_TOKEN'])") }

    $sizeHdr = & curl.exe -sIL @auth "https://huggingface.co/$Repo/resolve/main/$File" |
        Select-String -Pattern 'x-linked-size:\s*(\d+)' | Select-Object -First 1
    if ($sizeHdr) {
        $gb = [math]::Round([long]$sizeHdr.Matches[0].Groups[1].Value / 1GB, 1)
        Write-Host "[ .. ] downloading $File ($gb GB) from $Repo"
    } else {
        Write-Host "[ .. ] downloading $File from $Repo"
    }

    $part = "$path.part"
    & curl.exe -sSfL -C - --retry 5 --retry-delay 3 @auth -o $part "https://huggingface.co/$Repo/resolve/main/$File"
    if ($LASTEXITCODE -ne 0) { throw "download of $File failed (curl exit $LASTEXITCODE)" }
    if (-not (Test-Path $part) -or (Get-Item $part).Length -lt 1GB) {
        throw "$File came back smaller than 1 GB - not a real model; delete data\models\$File.part and retry"
    }
    Move-Item $part $path -Force
    Write-Host "[ OK ] $File downloaded"
}

if ($Preset) {
    # Router mode: "; Download: repo -> file" comments sit right above the
    # [section] they belong to. Only the default section (MODEL_ALIAS)
    # blocks the start; other presets load on request, so a missing file
    # there is a warning, not a 20 GB download nobody asked for. When
    # MODEL_ALIAS names no section, fetch everything (fresh preset).
    $Entries = @(); $Pending = @()
    Get-Content $PresetPath | ForEach-Object {
        if ($_ -match '^\s*;\s*Download:\s*(\S+)\s*->\s*(\S+)') {
            $Pending += ,@($Matches[1], $Matches[2])
        } elseif ($_ -match '^\[([^\]]+)\]') {
            $Section = $Matches[1]
            foreach ($p in $Pending) { $Entries += [pscustomobject]@{ Section = $Section; Repo = $p[0]; File = $p[1] } }
            $Pending = @()
        }
    }
    $All = -not ($Entries | Where-Object Section -eq $Alias)
    foreach ($e in $Entries) {
        if (Test-Path (Join-Path $ModelDir $e.File)) { continue }
        if ($All -or $e.Section -eq $Alias) { Ensure-Model $e.Repo $e.File }
        else { Write-Host "[WARN] preset [$($e.Section)] is missing $($e.File) - fetch it with: ./golem pull $($e.Section)" }
    }
} else {
    Ensure-Model $ModelRepo $ModelFile
    if ($DraftFile) { Ensure-Model $DraftRepo $DraftFile }
}

# A preset section without a "; Download:" comment would only fail at
# request time (file not found) - catch it now, with the section named.
if ($Preset) {
    # Only files with no Download source at all are fatal; known-but-not-
    # fetched presets were warned about above.
    $missing = Get-Content $PresetPath | ForEach-Object {
        if ($_ -match '^\s*(model|spec-draft-model)\s*=\s*(\S+)') { $Matches[2] }
    } | Where-Object { -not (Test-Path (Join-Path $ModelDir $_)) -and -not ($Entries.File -contains $_) }
    if ($missing) { throw "Preset models without a `"; Download:`" source (add one per file in $Preset): $($missing -join ', ')" }
}

# ------------------------------------------------------------
# Launch (flags mirror compose.yml)
# ------------------------------------------------------------

$LogDir = Join-Path $Root "logs"
New-Item -ItemType Directory -Force $LogDir | Out-Null
$Log = Join-Path $LogDir "llama-server-windows.log"

# Every flag is collected under its canonical (llama.cpp) name, value
# "true" = bare switch, then translated by the engine adapter on the way
# out (see engines\llama.cpp\engine.ini for the flag.* / map.* keys).
$Flags = [ordered]@{
    "host" = $BindHost; "port" = $Port; "threads" = $Threads; "parallel" = $Parallel
    "metrics" = "true"; "api-key" = $ApiKey; "log-colors" = "off"
    # Engines that append ".log" themselves get the path without it.
    "log-file" = $(if ($Eng["log_file_adds_ext"] -eq "true") { $Log -replace '\.log$', '' } else { $Log })
}
if ($SleepIdle) { $Flags["sleep-idle-seconds"] = $SleepIdle }

# Engines without router mode serve one model: the default section's flags
# ([*] merged with [MODEL_ALIAS]) become the command line. Strata skips
# this entirely: its launch below runs its own config, not these flags.
$Router = $Eng["router"] -ne "false"
if ($Preset -and -not $Router -and $Eng["launcher"] -ne "strata") {
    $Flags["alias"] = $Alias
    $section = ""; $found = $false
    Get-Content $PresetPath | ForEach-Object {
        if ($_ -match '^\[([^\]]+)\]') { $section = $Matches[1]; if ($section -eq $Alias) { $found = $true } }
        elseif ($_ -match '^\s*([A-Za-z0-9\-]+)\s*=\s*(.*?)\s*$' -and ($section -eq "*" -or $section -eq $Alias)) {
            $Flags[$Matches[1]] = $Matches[2]
        }
    }
    if (-not $found) { throw "Engine $EngineName has no router mode and preset section [$Alias] (MODEL_ALIAS) is not in $Preset" }
    Write-Host "[ .. ] $EngineName has no router mode: serving only [$Alias]"
} elseif ($Preset) {
    # Router: command-line flags override every preset, so only the shared
    # ones above go here; one model resident at a time (12 GB card).
    $Flags["models-preset"] = $PresetPath; $Flags["models-max"] = "1"
} else {
    $Flags["model"] = $ModelPath; $Flags["alias"] = $Alias; $Flags["ctx-size"] = $Ctx
    $Flags["n-gpu-layers"] = $GpuLayers; $Flags["jinja"] = "true"; $Flags["flash-attn"] = "on"
    $Flags["cache-type-k"] = $CacheK; $Flags["cache-type-v"] = $CacheV; $Flags["load-mode"] = $LoadMode
    if ($SpecType) { $Flags["spec-type"] = $SpecType }
    if ($DraftFile) { $Flags["spec-draft-model"] = (Join-Path $ModelDir $DraftFile) }
}

# flag.<key> = --name renames, flag.<key> = - drops (warned);
# map.<key>.<value> = <args> replaces the whole flag (- drops it).
$ServerArgs = @()
if ($Eng["launcher"] -eq "strata") {
    # Strata's server takes no engine flags: the strata-*.json its setup
    # wrote carries the model and engine arguments. Golem maps what it can:
    # the idle timer becomes --idle-unload. Clients ask for the model by
    # the json's "model_name".
    $json = Get-ChildItem $BinDir -Filter "strata-*.json" |
        Where-Object { $_.Name -notmatch "^strata-vision" } | Select-Object -First 1
    if (-not $json) { throw "No strata-*.json in $BinDir - run ./golem up again to redo the setup" }
    $StrataModel = ([regex]::Match((Get-Content $json.FullName -Raw), '"model_name"\s*:\s*"([^"]+)"')).Groups[1].Value
    $ServerArgs = @((Join-Path $BinDir "serve\server.py"), "--engine", "strata",
                    "--config", $json.FullName, "--port", $Port)
    if ($SleepIdle) { $ServerArgs += @("--idle-unload", $SleepIdle) }
    $WorkDir = $BinDir
    Write-Host "[ .. ] Strata serves the model its setup installed ($StrataModel); set MODEL_ALIAS to that name"
} else {
    foreach ($k in $Flags.Keys) {
        $v = "$($Flags[$k])"
        if ($v -eq "false") { continue }
        if ($Eng.Contains("map.$k.$v")) {
            $repl = $Eng["map.$k.$v"]
            if ($repl -eq "-") { Write-Host "[WARN] $EngineName has no equivalent of --$k $v, dropped"; continue }
            $ServerArgs += @($repl -split '\s+' | Where-Object { $_ })
            continue
        }
        $name = if ($Eng.Contains("flag.$k")) { $Eng["flag.$k"] } else { "--$k" }
        if ($name -eq "-") { Write-Host "[WARN] $EngineName has no equivalent of --$k, dropped"; continue }
        if ($v -eq "true") { $ServerArgs += $name } else { $ServerArgs += @($name, $v) }
    }
    # EXTRA_ARGS are passed verbatim: they are already in the engine's syntax.
    if ($ExtraArgs -and -not $Preset) { $ServerArgs += ($ExtraArgs -split '\s+' | Where-Object { $_ }) }
    $WorkDir = $ModelDir
}
# Start-Process joins arguments with bare spaces: quote paths that have them.
$ServerArgs = $ServerArgs | ForEach-Object { if ("$_" -match '\s') { "`"$_`"" } else { "$_" } }

if ($DryRun) {
    Write-Host "[ OK ] dry run: `"$Server`" $($ServerArgs -join ' ')"
    Write-Host "       (working directory: $WorkDir)"
    exit 0
}

# The server writes its own log (--log-file) instead of -Redirect*: those
# make Start-Process spawn with handle inheritance, so llama-server would
# keep the caller's stdout pipe open and hang anything reading it until
# EOF (e.g. `./golem up | tail`). Strata's Python server has no log-file
# flag, so its stdout/stderr go to files after all (read them in logs\);
# its engine also logs via the "log" path in the config json.
# llama-server starts from data\models so preset model paths resolve as
# bare filenames.
if ($Eng["launcher"] -eq "strata") {
    $Proc = Start-Process -FilePath $Server -ArgumentList $ServerArgs -WorkingDirectory $WorkDir -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput (Join-Path $LogDir "strata-server.out.log") `
        -RedirectStandardError (Join-Path $LogDir "strata-server.err.log")
} else {
    $Proc = Start-Process -FilePath $Server -ArgumentList $ServerArgs -WorkingDirectory $WorkDir -WindowStyle Hidden -PassThru
}
New-Item -ItemType Directory -Force (Join-Path $Root ".local") | Out-Null
Set-Content -Path (Join-Path $Root ".local\llama-server-windows.pid") -Value $Proc.Id
Write-Host "[ OK ] $EngineName $WinBuild started, PID $($Proc.Id)"
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
        throw "$EngineName server exited during startup (exit code $($Proc.ExitCode))"
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

if ($Preset -and $Router) {
    Write-Host "[ OK ] router healthy on http://$BindHost`:$Port (presets: $Preset; models load on first request)"
} else {
    Write-Host "[ OK ] healthy on http://$BindHost`:$Port (model: $(if ($Preset) { $Alias } else { $ModelFile }))"
}
Write-Host ""
Write-Host "Point Claude Code at Golem:"
Write-Host "`$env:ANTHROPIC_BASE_URL = `"http://127.0.0.1:$Port`""
Write-Host "`$env:ANTHROPIC_AUTH_TOKEN = `"$ApiKey`""
Write-Host "`$env:ANTHROPIC_MODEL = `"$Alias`""
Write-Host "`$env:ANTHROPIC_SMALL_FAST_MODEL = `"$Alias`""

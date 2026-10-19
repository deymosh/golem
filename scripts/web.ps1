# Golem - web UI: live benchmarks (results.jsonl) + start/stop/bench/eval.
#
#   powershell -ExecutionPolicy Bypass -File scripts\web.ps1 [-Port 8787]
#   (or: golem.cmd web / ./golem web)
#
# Serves only 127.0.0.1. The page is bench\report.template.html with the
# records inlined (same as ./golem report); in the browser it then polls
# /results.jsonl every 15 s, so the jsonl file stays the single source of
# truth. POST /api/run launches one of a fixed set of ./golem commands in
# the background (no free-form commands); GET /api/job tails its output.
# Needs Git Bash (bash.exe) for bench/eval/pull; start/stop/restart/status
# work without it via golem.cmd.

param([int]$Port = 8787)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$Root = Split-Path -Parent $PSScriptRoot
$Results = Join-Path $Root "bench\results.jsonl"
$Template = Join-Path $Root "bench\report.template.html"
$WebLog = Join-Path $Root "logs\web-actions.log"
$Env_ = Join-Path $Root ".env"

# ---- page: template + records inlined (regenerated per request) --------
function Get-Page() {
    $lines = @()
    if (Test-Path $Results) { $lines = [IO.File]::ReadAllLines($Results) | Where-Object { $_.Trim().StartsWith("{") } }
    $data = ("[" + ($lines -join ",`n") + "]").Replace("</", "<\/")
    [IO.File]::ReadAllText($Template).Replace("/*GOLEM_DATA*/", $data)
}

# ---- helpers -----------------------------------------------------------
function Env-Get([string]$k) {
    foreach ($l in Get-Content $Env_) { if ($l -match "^\s*$k\s*=\s*`"?([^`"]*)`"?") { return $Matches[1] } }
    return $null
}
function Env-Set([string]$k, [string]$v) {
    $out = @(Get-Content $Env_) | ForEach-Object { if ($_ -match "^\s*#?\s*$k\s*=") { "$k=`"$v`"" } else { $_ } }
    if (-not ($out -match "^\s*$k\s*=")) { $out += "$k=`"$v`"" }
    Set-Content $Env_ $out
}
function Presets() {
    Get-Content (Join-Path $Root "config\models.ini") | ForEach-Object { if ($_ -match "^\[([^\]]+)\]" -and $Matches[1] -ne "*") { $Matches[1] } }
}
function Engines() {
    Get-ChildItem (Join-Path $Root "engines") -Directory | ForEach-Object { $_.Name }
}
function Json([hashtable]$h) {
    $p = $h.GetEnumerator() | Sort-Object Name | ForEach-Object {
        $v = $_.Value
        if ($v -is [bool]) { $v = if ($v) { "true" } else { "false" } }
        elseif ($v -is [array]) { $v = "[" + (($v | ForEach-Object { '"'+([string]$_).Replace('\','\\').Replace('"','\"')+'"' }) -join ",") + "]" }
        elseif ($v -isnot [int] -and $v -isnot [double]) { $v = '"' + ([string]$v).Replace('\','\\').Replace('"','\"').Replace("`r?`n", "\n") + '"' }
        '"{0}":{1}' -f $_.Name, $v
    }
    "{" + ($p -join ",") + "}"
}

# ---- background jobs: one at a time, fixed commands only ---------------
$script:Job = @{ running = $false; action = ""; log = ""; started = $null }
function Start-Job2([string]$action, [string]$arg) {
    if ($script:Job.running) { return $false }
    # Every command ends with a sentinel line: MSYS bash re-execs itself, so
    # the process object Start-Process returns can report HasExited while
    # the real job still runs. The sentinel is what marks it done. In bash
    # the separator must be ";" - "&" would background the job instead.
    $shDone = "; echo __GOLEM_JOB_DONE__"
    $cmdDone = " & echo __GOLEM_JOB_DONE__"
    $cmd = $null; $arglist = $null
    switch ($action) {
        "start"   { $cmd = "cmd.exe"; $arglist = "/c golem.cmd start $cmdDone" }
        "stop"    { $cmd = "cmd.exe"; $arglist = "/c golem.cmd stop $cmdDone" }
        "restart" { $cmd = "cmd.exe"; $arglist = "/c golem.cmd restart $cmdDone" }
        "status"  { $cmd = "cmd.exe"; $arglist = "/c golem.cmd status $cmdDone" }
        default   {
            $bash = (Get-Command bash -ErrorAction SilentlyContinue).Source
            if (-not $bash) { return $false }
            # -c, not -lc: a login shell sources the user's profile, which on
            # this machine sets a comma-decimal locale and breaks the scripts.
            switch ($action) {
                "bench" { $arglist = "-c `"export LC_ALL=C; cd '$Root' && ./golem bench$shDone`"" }
                "eval"  { $arglist = "-c `"export LC_ALL=C; cd '$Root' && ./golem eval --long 200000$shDone`"" }
                "pull"  { if ($arg -notmatch "^[A-Za-z0-9._-]+$") { return $false }
                          $arglist = "-c `"export LC_ALL=C; cd '$Root' && ./golem pull $arg$shDone`"" }
                default { return $false }
            }
            $cmd = $bash
        }
    }
    $script:Job.running = $true; $script:Job.action = $action; $script:Job.started = Get-Date
    New-Item -ItemType Directory -Force (Split-Path $WebLog) | Out-Null
    Set-Content $WebLog "=== $action $(Get-Date -Format s)"
    # The process object is polled (see Update-Job) rather than watched with
    # Register-ObjectEvent: an event action runs in its own scope, where
    # $script: is not this script's, so the flag never came back.
    $script:Job.proc = Start-Process -FilePath $cmd -ArgumentList $arglist -WorkingDirectory $Root `
        -WindowStyle Hidden -PassThru -RedirectStandardOutput "$WebLog.out" -RedirectStandardError "$WebLog.err"
    return $true
}

# Fold a finished job's output into the log and clear the flag.
function Update-Job() {
    if (-not $script:Job.running) { return }
    $out = "$($WebLog).out"
    if (-not (Test-Path $out)) { return }
    if (-not (Select-String -Path $out -Pattern "__GOLEM_JOB_DONE__" -Quiet -ErrorAction SilentlyContinue)) { return }
    Get-Content $out, "$($WebLog).err" -ErrorAction SilentlyContinue | Add-Content $WebLog
    $script:Job.running = $false; $script:Job.proc = $null
}
function Job-Tail([int]$n = 14) {
    if ($script:Job.running -and (Test-Path "$($WebLog).out")) {
        return @(Get-Content "$($WebLog).out" -Tail $n -ErrorAction SilentlyContinue) +
               @(Get-Content "$($WebLog).err" -Tail 4 -ErrorAction SilentlyContinue)
    }
    return @(Get-Content $WebLog -Tail $n -ErrorAction SilentlyContinue)
}

# ---- minimal HTTP over TcpListener (localhost, no admin needed) --------
$listener = New-Object System.Net.Sockets.TcpListener([Net.IPAddress]::Loopback, $Port)
$listener.Start()
Write-Host "[ OK ] golem web UI on http://127.0.0.1:$Port  (Ctrl+C to stop)"

function Read-Request($stream) {
    $buf = New-Object byte[] 65536
    $n = $stream.Read($buf, 0, $buf.Length)
    if ($n -le 0) { return $null }
    $text = [Text.Encoding]::ASCII.GetString($buf, 0, $n)
    $head, $body = $text -split "`r`n`r`n", 2
    $lines = $head -split "`r`n"
    $parts = $lines[0] -split " "
    return @{ method = $parts[0]; path = $parts[1]; body = $body }
}
function Send($stream, [string]$type, [byte[]]$bytes) {
    $h = [Text.Encoding]::ASCII.GetBytes("HTTP/1.1 200 OK`r`nContent-Type: $type`r`nContent-Length: $($bytes.Length)`r`nConnection: close`r`n`r`n")
    $stream.Write($h, 0, $h.Length); $stream.Write($bytes, 0, $bytes.Length)
}

$utf8 = New-Object Text.UTF8Encoding($false)
while ($true) {
    $client = $listener.AcceptTcpClient()
    try {
        $client.ReceiveTimeout = 5000
        $req = Read-Request $client.GetStream()
        if (-not $req) { $client.Close(); continue }
        $path = $req.path
        if ($req.method -eq "GET" -and ($path -eq "/" -or $path -eq "/index.html")) {
            Send $client.GetStream() "text/html; charset=utf-8" ($utf8.GetBytes((Get-Page)))
        }
        elseif ($req.method -eq "GET" -and $path -eq "/results.jsonl") {
            $b = if (Test-Path $Results) { [IO.File]::ReadAllBytes($Results) } else { $utf8.GetBytes("") }
            Send $client.GetStream() "application/x-ndjson" $b
        }
        elseif ($req.method -eq "GET" -and $path -eq "/api/state") {
            $alias = Env-Get "MODEL_ALIAS"; $engine = Env-Get "ENGINE"; if (-not $engine) { $engine = "llama.cpp" }
            $up = & curl.exe -s -m 2 --noproxy "*" -o NUL -w "%{http_code}" "http://127.0.0.1:$(Env-Get 'LOCAL_AI_PORT')/health"
            $vram = (& nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits 2>$null) -replace "[\s%]", ""
            Update-Job
            $tail = if ($script:Job.running -or (Test-Path $WebLog)) { (Job-Tail 14) -join "`n" } else { "" }
            Send $client.GetStream() "application/json" ($utf8.GetBytes((Json @{ up = ($up -eq "200"); vram = $vram; alias = $alias; engine = $engine; presets = @(Presets); engines = @(Engines); job_running = $script:Job.running; job_action = $script:Job.action; job_tail = $tail })))
        }
        elseif ($req.method -eq "POST" -and $path -eq "/api/run") {
            $form = @{}
            foreach ($kv in ($req.body -split "&")) { $k, $v = $kv -split "=", 2; $form[$k] = [URI]::UnescapeDataString(($v -replace "\+", " ")) }
            if ($form.action -eq "alias" -and $form.arg -and (Presets) -contains $form.arg) { Env-Set "MODEL_ALIAS" $form.arg; $ok = "applied" }
            elseif ($form.action -eq "engine" -and $form.arg -and (Engines) -contains $form.arg) { Env-Set "ENGINE" $form.arg; $ok = "applied" }
            else { $ok = if (Start-Job2 $form.action $form.arg) { "started" } else { "busy-or-unknown" } }
            Send $client.GetStream() "application/json" ($utf8.GetBytes(('{"result":"' + $ok + '"}')))
        }
        else {
            $b = $utf8.GetBytes("404")
            $h = [Text.Encoding]::ASCII.GetBytes("HTTP/1.1 404 Not Found`r`nContent-Length: 3`r`nConnection: close`r`n`r`n")
            $s = $client.GetStream(); $s.Write($h, 0, $h.Length); $s.Write($b, 0, 3)
        }
    } catch { Write-Host "[WARN] request: $($_.Exception.Message)" }
    finally { try { $client.Close() } catch {} }
}

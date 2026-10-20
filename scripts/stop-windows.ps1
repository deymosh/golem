# Golem - stop the native Windows llama-server started by start-windows.ps1.
#
# Usage:  powershell -ExecutionPolicy Bypass -File scripts\stop-windows.ps1

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
$PidFile = Join-Path $Root ".local\llama-server-windows.pid"

if (-not (Test-Path $PidFile)) {
    Write-Host "No pid file at $PidFile - nothing to stop."
    exit 0
}

$procId = Get-Content $PidFile
$proc = Get-Process -Id $procId -ErrorAction SilentlyContinue
if (-not $proc) {
    Write-Host "Process $procId is not running - cleaning up pid file."
    Remove-Item $PidFile -Force
    exit 0
}

# /T: in router mode each model runs in a child llama-server that
# Stop-Process would orphan (still holding VRAM and the model in RAM).
taskkill.exe /T /F /PID $procId | Out-Null
Remove-Item $PidFile -Force

# Wait for the card, not just for the kill. taskkill returns when the kill is
# *issued*; the driver reclaims the VRAM a moment later, and a start that
# begins in between fails with "cudaMalloc failed: out of memory" while
# reporting a perfectly healthy pid. A loaded 35B holds ~11 GB and the desktop
# holds under 2, so "under 2 GB" means the model is gone. Bounded, and silent
# on a machine with no nvidia-smi (the loop exits on the first empty reading).
$settled = $false
for ($i = 0; $i -lt 30; $i++) {
    Start-Sleep -Milliseconds 500
    $used = (nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits 2>$null |
             Select-Object -First 1)
    if (-not $used -or [int]$used -lt 2048) { $settled = $true; break }
}
if (-not $settled) {
    Write-Host "[WARN] the GPU still reports memory in use after 15 s - the next start may fail"
}

Write-Host "[ OK ] llama-server (PID $procId) stopped."

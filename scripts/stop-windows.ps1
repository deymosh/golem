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
Write-Host "[ OK ] llama-server (PID $procId) stopped."

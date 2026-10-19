# Golem - build bench/report.html from bench/results.jsonl (Windows, no Git
# Bash needed; PowerShell 5.1+). Inlines the records into the template, so
# the page is one self-contained file that opens from disk.
#
#   powershell -File scripts\report.ps1 [-NoOpen]

param([switch]$NoOpen)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
$Results = Join-Path $Root "bench\results.jsonl"
$Template = Join-Path $Root "bench\report.template.html"
$Out = Join-Path $Root "bench\report.html"

$Utf8 = New-Object System.Text.UTF8Encoding($false)
$Lines = @()
if (Test-Path $Results) {
    $Lines = [IO.File]::ReadAllLines($Results, $Utf8) | Where-Object { $_.Trim().StartsWith("{") }
}
# "</" inside a <script> block would end it early; "<\/" is the same JSON.
$Data = "[" + ($Lines -join ",`n") + "]"
$Data = $Data.Replace("</", "<\/")

$Html = [IO.File]::ReadAllText($Template, $Utf8).Replace("/*GOLEM_DATA*/", $Data)
[IO.File]::WriteAllText($Out, $Html, $Utf8)
Write-Host "[ OK ] $Out ($($Lines.Count) runs)"
if (-not $NoOpen) { Start-Process $Out }

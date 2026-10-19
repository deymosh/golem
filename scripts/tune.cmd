@echo off
rem Golem - auto-tune the offload split (runs the bash tuner via Git Bash).
rem
rem   scripts\tune.cmd [preset] [--dry-run] [--candidates 30,32,34,36]
rem
rem The tuner is a bash script (it must restart the server between
rem candidates); this only finds bash and hands over, so `golem.cmd tune`
rem works without the user opening Git Bash.

setlocal
set "ROOT=%~dp0.."
set "BASH="
for /f "delims=" %%b in ('where bash 2^>nul') do if not defined BASH set "BASH=%%b"
if not defined BASH (
    echo   [ERROR] Git Bash ^(bash.exe^) not found - auto-tune needs it.
    echo           Install Git for Windows, or use the web console's Tune button.
    exit /b 1
)
"%BASH%" -c "export LC_ALL=C; cd \"$(cygpath -u '%ROOT%')\"; ./golem tune %*"

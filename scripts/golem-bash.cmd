@echo off
rem Golem - run a bash-only golem command from cmd.exe.
rem
rem   scripts\golem-bash.cmd <command> [args...]
rem   e.g. golem-bash.cmd bench --runs 3
rem        golem-bash.cmd tune ornith-1.5-35b
rem
rem bench, eval and tune are shell scripts; this finds Git Bash and hands
rem over, so golem.cmd can offer them without the user opening a shell.
rem
rem It must be Git Bash, not the WSL stub in System32 (which has no cygpath
rem and cannot see C:\ paths), so the Git install is looked for first.

setlocal
set "ROOT=%~dp0.."
set "BASH="
if exist "%ProgramFiles%\Git\bin\bash.exe" set "BASH=%ProgramFiles%\Git\bin\bash.exe"
if not defined BASH if exist "%ProgramFiles(x86)%\Git\bin\bash.exe" set "BASH=%ProgramFiles(x86)%\Git\bin\bash.exe"
if not defined BASH if exist "%LOCALAPPDATA%\Programs\Git\bin\bash.exe" set "BASH=%LOCALAPPDATA%\Programs\Git\bin\bash.exe"
if not defined BASH (
    for /f "delims=" %%b in ('where bash 2^>nul') do (
        echo %%b | find /i "\Git\" >nul && if not defined BASH set "BASH=%%b"
    )
)
if not defined BASH (
    echo   [ERROR] Git Bash ^(bash.exe^) not found - this command needs it.
    echo           Install Git for Windows, or use the web console's buttons.
    exit /b 1
)
"%BASH%" -c "export LC_ALL=C; cd '%ROOT:\=/%' && ./golem %*"

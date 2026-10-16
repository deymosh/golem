@echo off
rem Golem - Windows launcher (windows deploy mode, no Git Bash needed).
rem
rem   golem.cmd                 interactive menu (double-click / desktop shortcut)
rem   golem.cmd start|stop|restart|status|logs
rem
rem Wraps scripts\start-windows.ps1 / stop-windows.ps1 and reads the same .env.
rem First run bootstraps itself: .env from .env.example, the llama.cpp
rem binary and any missing models download automatically.
rem
rem The menu is ANSI-colored: Windows Terminal and every VT-enabled console
rem (the default on Windows 11) render it; a legacy conhost without VT would
rem show stray escape bytes instead of colors.

setlocal EnableExtensions EnableDelayedExpansion

rem "prompt $E" piped through a child cmd prints the ESC byte - the classic
rem no-powershell way to get it into a variable for ANSI sequences.
set "ESC="
for /f "delims=" %%E in ('echo prompt $E ^| cmd 2^>nul') do set "ESC=%%E"
rem the child cmd prints ESC plus trailing whitespace - keep byte 1 only,
rem or every color sequence turns into "ESC [96m" and renders as garbage.
if defined ESC set "ESC=!ESC:~0,1!"

set "ROOT=%~dp0"
set "PS=powershell.exe -NoProfile -ExecutionPolicy Bypass"

rem Zero-touch first run: no .env yet -> make one from the example,
rem flipped to windows mode (this launcher IS windows mode). The start
rem script below then downloads llama.cpp and any missing models, so a
rem fresh clone goes from double-click to running server on its own.
if not exist "%ROOT%.env" (
    if not exist "%ROOT%.env.example" (
        echo   [ERROR] no .env and no .env.example found
        exit /b 1
    )
    copy /y "%ROOT%.env.example" "%ROOT%.env" >nul
    rem [char]34 = ": the powershell.exe CLI eats literal quotes inside
    rem a quoted -Command argument, so build them in PowerShell instead.
    %PS% -Command "$q=[char]34; (Get-Content -Raw '%ROOT%.env') -replace ('DEPLOY_MODE='+$q+'docker'), ('DEPLOY_MODE='+$q+'windows') | Set-Content -NoNewline '%ROOT%.env'"
    echo   [ OK ] created .env for windows mode - edit it to change the model
)

set "PORT=8000"
set "KEY="
set "ALIAS=local-model"
set "PRESET="
for /f "usebackq tokens=1,* delims==" %%a in ("%ROOT%.env") do (
    if "%%a"=="LOCAL_AI_PORT" set "PORT=%%~b"
    if "%%a"=="LOCAL_AI_API_KEY" set "KEY=%%~b"
    if "%%a"=="MODEL_ALIAS" set "ALIAS=%%~b"
    if "%%a"=="MODELS_PRESET" set "PRESET=%%~b"
)

if "%~1"=="" goto menu
for %%c in (start stop restart status logs) do if /i "%~1"=="%%c" (call :%%c & exit /b)
echo Usage: golem.cmd [start^|stop^|restart^|status^|logs]
exit /b 1

:menu
call :banner
choice /c 12345Q /n /m "  > "
if errorlevel 6 exit /b 0
if errorlevel 5 (call :logs & goto menu)
if errorlevel 4 (call :status & goto menu)
if errorlevel 3 (call :restart & goto menu)
if errorlevel 2 (call :stop & goto menu)
call :start
goto menu

rem ------------------------------------------------------------
rem UI
rem ------------------------------------------------------------

:banner
echo.
echo !ESC![96m   ###     ###    #      #####   #   #!ESC![0m
echo !ESC![96m  #       #   #   #      #       ##  ##!ESC![0m
echo !ESC![96m  #  ##   #   #   #      ####    # # #!ESC![0m
echo !ESC![96m  #   #   #   #   #      #       #   #!ESC![0m
echo !ESC![96m   ###     ###   #####   #####   #   #!ESC![0m
echo.
echo   !ESC![1mlocal AI construct for coding agents!ESC![0m  !ESC![90mAnthropic + OpenAI, zero tokens off the LAN!ESC![0m
echo   !ESC![90m--------------------------------------------------------------!ESC![0m
echo   endpoint  !ESC![93mhttp://127.0.0.1:%PORT%!ESC![0m        model  !ESC![93m%ALIAS%!ESC![0m
if defined PRESET echo   !ESC![90mrouter mode: the request picks the model from %PRESET%!ESC![0m
call :statusline
echo.
echo   !ESC![93m1!ESC![0m  Start         !ESC![90mboot the forge - installs and downloads what is missing!ESC![0m
echo   !ESC![93m2!ESC![0m  Stop
echo   !ESC![93m3!ESC![0m  Restart
echo   !ESC![93m4!ESC![0m  Status        !ESC![90mresident models + VRAM!ESC![0m
echo   !ESC![93m5!ESC![0m  Logs          !ESC![90mfollow in a new window!ESC![0m
echo   !ESC![90mQ!ESC![0m  Quit
echo.
exit /b 0

rem One-line live health read; /health does not reset the idle-sleep timer.
:statusline
curl.exe -fs --noproxy "*" -m 2 -o NUL "http://127.0.0.1:%PORT%/health" 2>nul
if errorlevel 1 (
    echo   status    !ESC![91m[ -- ] stopped!ESC![0m
) else (
    echo   status    !ESC![92m[ OK ] running!ESC![0m
)
exit /b 0

rem ------------------------------------------------------------
rem Actions
rem ------------------------------------------------------------

:start
%PS% -File "%ROOT%scripts\start-windows.ps1"
exit /b 0

:stop
%PS% -File "%ROOT%scripts\stop-windows.ps1"
exit /b 0

:restart
call :stop
call :start
exit /b 0

:status
rem /health and /models don't reset the idle-sleep timer (/props?model= would).
curl.exe -fs --noproxy "*" -m 2 -o NUL "http://127.0.0.1:%PORT%/health"
if errorlevel 1 (
    echo   !ESC![91m[ -- ]!ESC![0m not running
    exit /b 0
)
echo   !ESC![92m[ OK ]!ESC![0m running
curl.exe -s --noproxy "*" -m 5 -H "Authorization: Bearer %KEY%" "http://127.0.0.1:%PORT%/models" ^
  | %PS% -Command "$m = ($input -join '') | ConvertFrom-Json; foreach ($d in $m.data) { $s = if ($d.status) { $d.status.value } else { 'loaded' }; '         {0,-18} {1}' -f $d.id, $s }"
for /f "tokens=1" %%v in ('nvidia-smi --query-gpu^=memory.used --format^=csv^,noheader^,nounits 2^>nul') do echo          VRAM in use: %%v MiB
exit /b 0

:logs
start "golem logs" %PS% -Command "Get-Content -Wait -Tail 40 '%ROOT%logs\llama-server-windows.log'"
exit /b 0

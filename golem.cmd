@echo off
rem Golem - Windows launcher (windows deploy mode, no Git Bash needed).
rem
rem   golem.cmd                 interactive menu (double-click / desktop shortcut)
rem   golem.cmd start|stop|restart|status|logs|report
rem
rem Wraps scripts\start-windows.ps1 / stop-windows.ps1 and reads the same .env.
rem First run bootstraps itself: .env from .env.example, the llama.cpp
rem binary and any missing models download automatically.

setlocal EnableExtensions
set "ROOT=%~dp0"
set "PS=powershell.exe -NoProfile -ExecutionPolicy Bypass"

rem Zero-touch first run: no .env yet -> make one from the example,
rem flipped to windows mode (this launcher IS windows mode). The start
rem script below then downloads llama.cpp and any missing models, so a
rem fresh clone goes from double-click to running server on its own.
if not exist "%ROOT%.env" (
    if not exist "%ROOT%.env.example" (
        echo   [ERROR] no .env and no .env.example found & exit /b 1
    )
    copy /y "%ROOT%.env.example" "%ROOT%.env" >nul
    rem [char]34 = ": the powershell.exe CLI eats literal quotes inside
    rem a quoted -Command argument, so build them in PowerShell instead.
    %PS% -Command "$q=[char]34; (Get-Content -Raw '%ROOT%.env') -replace ('DEPLOY_MODE='+$q+'docker'), ('DEPLOY_MODE='+$q+'windows') | Set-Content -NoNewline '%ROOT%.env'"
    echo   [ OK ] created .env for windows mode - edit it to change the model
)

set "PORT=8000"
set "KEY="
for /f "usebackq tokens=1,* delims==" %%a in ("%ROOT%.env") do (
    if "%%a"=="LOCAL_AI_PORT" set "PORT=%%~b"
    if "%%a"=="LOCAL_AI_API_KEY" set "KEY=%%~b"
)

rem No argument: the TUI is the default face (menu: golem.cmd menu).
if "%~1"=="" goto tui
for %%c in (start stop restart status logs report web tune bench eval tui menu) do if /i "%~1"=="%%c" (call :%%c & exit /b)
echo Usage: golem.cmd [start^|stop^|restart^|status^|logs^|report^|web^|tune^|bench^|eval^|tui]
exit /b 1

:menu
echo.
echo   golem :: http://127.0.0.1:%PORT%
echo   ------------------------------
echo   1  Start
echo   2  Stop
echo   3  Restart
echo   4  Status
echo   5  Logs (new window)
echo   6  Benchmark report (browser)
echo   7  Web UI (console: results + controls + models)
echo   8  Auto-tune (engine + offload split)
echo   9  Bench (speed)
echo   0  Eval (quality gate)
echo   T  Full-screen console (TUI)
echo   Q  Quit
echo.
choice /c 1234567890TQ /n /m "  > "
if errorlevel 12 exit /b 0
if errorlevel 11 (call :tui & goto menu)
if errorlevel 10 (call :eval & goto menu)
if errorlevel 9 (call :bench & goto menu)
if errorlevel 8 (call :tune & goto menu)
if errorlevel 7 (call :web & goto menu)
if errorlevel 6 (call :report & goto menu)
if errorlevel 5 (call :logs & goto menu)
if errorlevel 4 (call :status & goto menu)
if errorlevel 3 (call :restart & goto menu)
if errorlevel 2 (call :stop & goto menu)
call :start
goto menu

:start
%PS% -File "%ROOT%scripts\start-windows.ps1"
exit /b

:stop
%PS% -File "%ROOT%scripts\stop-windows.ps1"
exit /b

:restart
call :stop
call :start
exit /b

:status
rem /health and /models don't reset the idle-sleep timer (/props?model= would).
curl.exe -fs --noproxy "*" -m 2 -o NUL "http://127.0.0.1:%PORT%/health"
if errorlevel 1 (echo   [ -- ] not running & exit /b 0)
echo   [ OK ] running
curl.exe -s --noproxy "*" -m 5 -H "Authorization: Bearer %KEY%" "http://127.0.0.1:%PORT%/models" ^
  | %PS% -Command "$m = ($input -join '') | ConvertFrom-Json; foreach ($d in $m.data) { $s = if ($d.status) { $d.status.value } else { 'loaded' }; '         {0,-18} {1}' -f $d.id, $s }"
for /f "tokens=1" %%v in ('nvidia-smi --query-gpu^=memory.used --format^=csv^,noheader^,nounits 2^>nul') do echo          VRAM in use: %%v MiB
exit /b 0

:logs
start "golem logs" %PS% -Command "Get-Content -Wait -Tail 40 '%ROOT%logs\llama-server-windows.log'"
exit /b 0

:report
%PS% -File "%ROOT%scripts\report.ps1"
exit /b 0
:web
%PS% -File "%ROOT%scripts\web.ps1"
exit /b 0
:tune
call "%ROOT%scripts\tune.cmd" %*
exit /b 0
:bench
call "%ROOT%scripts\golem-bash.cmd" bench %*
exit /b 0
:eval
call "%ROOT%scripts\golem-bash.cmd" eval %*
exit /b 0
:menu
goto menu_show

:tui
rem The TUI needs a real terminal for raw keys and the mouse: Windows
rem Terminal first, mintty (shipped with Git for Windows) second, and the
rem plain bridge — same console, no mouse — as the last resort.
set "GBASH=%ProgramFiles%\Git\bin\bash.exe"
where wt.exe >nul 2>nul && if exist "%GBASH%" (
    start "" wt.exe -w -1 nt --title "Golem" "%GBASH%" -lc "cd '%ROOT:\=/%' && ./golem tui"
    exit /b 0
)
if exist "%ProgramFiles%\Git\usr\bin\mintty.exe" (
    start "" "%ProgramFiles%\Git\usr\bin\mintty.exe" -h always -e "%GBASH%" -lc "cd '%ROOT:\=/%' && ./golem tui"
    exit /b 0
)
call "%ROOT%scripts\golem-bash.cmd" tui
exit /b 0

:menu_show
call "%ROOT%scripts\golem-bash.cmd" tui %*
exit /b 0

@echo off
rem Golem - Windows launcher (windows deploy mode, no Git Bash needed).
rem
rem   golem.cmd                 interactive menu (double-click / desktop shortcut)
rem   golem.cmd start|stop|restart|status|logs
rem
rem Wraps scripts\start-windows.ps1 / stop-windows.ps1 and reads the same .env.

setlocal EnableExtensions
set "ROOT=%~dp0"
set "PS=powershell.exe -NoProfile -ExecutionPolicy Bypass"

set "PORT=8000"
set "KEY="
for /f "usebackq tokens=1,* delims==" %%a in ("%ROOT%.env") do (
    if "%%a"=="LOCAL_AI_PORT" set "PORT=%%~b"
    if "%%a"=="LOCAL_AI_API_KEY" set "KEY=%%~b"
)

if "%~1"=="" goto menu
for %%c in (start stop restart status logs) do if /i "%~1"=="%%c" (call :%%c & exit /b)
echo Usage: golem.cmd [start^|stop^|restart^|status^|logs]
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
echo   Q  Quit
echo.
choice /c 12345Q /n /m "  > "
if errorlevel 6 exit /b 0
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

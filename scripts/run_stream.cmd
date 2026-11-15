@echo off
REM ---------------------------------------------------------------------------
REM  MoreScreen stream service launcher
REM
REM  Usage:
REM      scripts\run_stream.cmd                              (default: primary monitor, WS 8787)
REM      scripts\run_stream.cmd --webrtc                     (WebRTC route A, recommended)
REM      scripts\run_stream.cmd --index 5                    (pick a display target)
REM      scripts\run_stream.cmd --index 0 --seconds 300      (any extra args pass through)
REM
REM  Route A (WebRTC) is the recommended transport: browser <video> + WebRTC.
REM  Open http://<host-ip>:8788/webrtc.html on the receiver device and tap Start.
REM
REM  Notes:
REM     - rs-host.exe is NOT on PATH; this script calls it by absolute path.
REM     - The service runs in the FOREGROUND: closing this window stops it.
REM     - After start, open the printed "receiver page" URL on your phone/tablet.
REM     - Keep this script ASCII-only on purpose: `chcp 65001` + Chinese text in
REM       batch echo lines is parsed incorrectly by cmd.exe.
REM ---------------------------------------------------------------------------
setlocal

set "ROOT=%~dp0.."
set "EXE=%LOCALAPPDATA%\Temp\rs-target\release\rs-host.exe"

if not exist "%EXE%" (
    set "EXE=%ROOT%\target\release\rs-host.exe"
)

if not exist "%EXE%" (
    echo [ERROR] rs-host.exe not found. Build it first from the project root:
    echo            cargo build --release -p remote-screen-host-win
    echo.
    pause
    exit /b 1
)

echo ============================================================
echo   MoreScreen stream service
echo   binary : %EXE%
echo   args   : %*
echo ============================================================
echo.

set "MODE=stream"
for %%A in (%*) do if /I "%%A"=="--webrtc" set "MODE=stream-webrtc"
"%EXE%" %MODE% %*

echo.
echo [service stopped]
pause

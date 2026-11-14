@echo off
REM ---------------------------------------------------------------
REM  One-shot VDD setup + verification (run in an ADMIN terminal)
REM  1) install driver package and CREATE the device node (/install)
REM  2) verify ROOT\MttVDD exists
REM  3) print result for the host tool to pick up
REM  ASCII-only content on purpose.
REM ---------------------------------------------------------------
set ROOT=D:\app-workbuddy\01_remote-screen
set INF=%ROOT%\tools\vdd-driver\VirtualDisplayDriver\MttVDD.inf
set LOG=%ROOT%\tools\vdd_setup.log

echo === VDD setup start === > "%LOG%"
echo INF: %INF% >> "%LOG%"
echo exists: >> "%LOG%" && dir /b "%INF%" >> "%LOG%" 2>&1

REM 1) install + create device node
pnputil /add-driver "%INF%" /install >> "%LOG%" 2>&1
echo PNPUTIL_EXIT=%ERRORLEVEL% >> "%LOG%"

REM 2) verify
echo --- device node --- >> "%LOG%"
pnputil /enum-devices /instanceid "ROOT\MttVDD" >> "%LOG%" 2>&1

echo --- display class --- >> "%LOG%"
pnputil /enum-devices /class Display >> "%LOG%" 2>&1

echo === VDD setup done === >> "%LOG%"
type "%LOG%"
echo.
echo If you see "ROOT\MttVDD" with status OK above, run:
echo   rs-host displays
echo and look for a NEW virtual display with a resolution (not 0x0).
echo If not found: reboot once, then run this script again.

@echo off
REM ---------------------------------------------------------------
REM  One-shot VDD setup (run in an ADMIN terminal)
REM
REM  KEY POINT: `pnputil /add-driver /install` ALONE does NOT create the
REM  device node. MttVDD.inf uses a root-enumerated software device
REM  (hardware id "Root\MttVDD"), and pnputil only installs drivers for
REM  devices that already exist. The node must be created explicitly via
REM  SetupAPI -- that is what `rs-host vdd-install` does.
REM
REM  ASCII-only content on purpose.
REM ---------------------------------------------------------------
set ROOT=D:\app-workbuddy\01_remote-screen
set INF=%ROOT%\tools\vdd-driver\VirtualDisplayDriver\MttVDD.inf
set PS1=%ROOT%\tools\create_vdd_device.ps1
set LOG=%ROOT%\tools\vdd_setup.log
set RLOG=%ROOT%\out\vdd_install.log

echo === VDD setup start === > "%LOG%"
echo INF: %INF% >> "%LOG%"

REM 1) publish driver package to DriverStore
pnputil /add-driver "%INF%" /install >> "%LOG%" 2>&1
echo PNPUTIL_EXIT=%ERRORLEVEL% >> "%LOG%"

REM 2) create the device node + bind driver (needs admin).
REM    Preferred path: rs-host vdd-install (SetupAPI, no WDK needed).
set EXE=%TEMP%\rs-target\release\rs-host.exe
if not exist "%EXE%" set EXE=%ROOT%\target\release\rs-host.exe
if exist "%EXE%" (
  "%EXE%" vdd-install --inf "%INF%" --log "%RLOG%" >> "%LOG%" 2>&1
  echo RSHOST_EXIT=%ERRORLEVEL% >> "%LOG%"
  type "%RLOG%" >> "%LOG%" 2>&1
) else (
  echo rs-host.exe not found, falling back to the PowerShell SetupAPI script >> "%LOG%"
  "%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "%PS1%" -InfPath "%INF%" >> "%LOG%" 2>&1
  echo PS1_EXIT=%ERRORLEVEL% >> "%LOG%"
)

REM 3) verification
echo --- device node --- >> "%LOG%"
pnputil /enum-devices /instanceid "ROOT\MTTVDD\0000" >> "%LOG%" 2>&1

echo --- display class --- >> "%LOG%"
pnputil /enum-devices /class Display >> "%LOG%" 2>&1

echo === VDD setup done === >> "%LOG%"
type "%LOG%"
echo.
echo If "ROOT\MTTVDD\0000" shows up with status OK, run:
echo   rs-host displays
echo and look for a NEW "Virtual Display Driver" entry with a real
echo resolution (not 0x0). If the node still does not appear, reboot
echo once and run this script again.

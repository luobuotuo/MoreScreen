@echo off
REM Install the static Virtual Display Driver (MikeTheTech, MIT) via pnputil.
REM ASCII-only content on purpose (no non-ASCII path/name in this script).
set LOG=D:\app-workbuddy\01_remote-screen\tools\vdd_install.log
echo === VDD install start === > "%LOG%"
echo User: %USERNAME% >> "%LOG%"
pnputil /add-driver "D:\app-workbuddy\01_remote-screen\tools\vdd-driver\VirtualDisplayDriver\MttVDD.inf" /install >> "%LOG%" 2>&1
echo PNPUTIL_EXIT=%ERRORLEVEL% >> "%LOG%"
echo === VDD install done === >> "%LOG%"

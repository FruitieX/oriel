@echo off
rem Development host: runs a local copy of the build on the share and
rem restarts with the new build whenever it changes. Set ORIEL_PIN to allow
rem pairing.
set SRC=%~dp0
set DST=%LOCALAPPDATA%\oriel-dev
set PAIR=
if not "%ORIEL_PIN%"=="" set PAIR=--pin %ORIEL_PIN%
:loop
robocopy "%SRC%." "%DST%" *.exe *.dll /NJH /NJS /NFL /NDL >nul
"%DST%\oriel.exe" host %PAIR% --log-file "%SRC%..\host.log" --restart-when-changed "%SRC%oriel.exe"
if %ERRORLEVEL%==75 goto loop
pause

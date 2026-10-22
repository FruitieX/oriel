@echo off
rem Development host: installs the build from this folder locally, starts it
rem in the tray and lets it update itself when a new build lands here. Set
rem ORIEL_PIN to allow pairing.
set SRC=%~dp0
set DST=%LOCALAPPDATA%\oriel-dev
set PAIR=
if not "%ORIEL_PIN%"=="" set PAIR=--pin %ORIEL_PIN%
robocopy "%SRC%." "%DST%" *.exe *.dll /NJH /NJS /NFL /NDL >nul
start "" "%DST%\oriel.exe" host %PAIR% --log-file "%SRC%..\host.log" --update-from "%SRC%."

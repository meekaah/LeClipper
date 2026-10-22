@echo off
setlocal
cd /d "%~dp0"

cargo build

echo.
if errorlevel 1 (
    echo Build FAILED.
) else (
    echo Build succeeded: %~dp0target\debug\screen2imgur.exe
)
echo.
pause

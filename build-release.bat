@echo off
setlocal
cd /d "%~dp0"

cargo build --release

echo.
if errorlevel 1 (
    echo Build FAILED.
) else (
    echo Build succeeded: %~dp0target\release\screen2imgur.exe
)
echo.
pause

@echo off
setlocal
set "cli=%~dp0codex-portable-lite.exe"
if not exist "%cli%" set "cli=%~dp0target\release\codex-portable-lite.exe"
if not exist "%cli%" (
    echo Build the CLI first with cargo build --release --locked.
    pause
    exit /b 1
)

if not "%~1"=="" set "CODEX_PORTABLE_LITE_DIR=%~1"
if defined CODEX_PORTABLE_LITE_DIR (
    "%cli%" update --install-dir "%CODEX_PORTABLE_LITE_DIR%"
) else (
    "%cli%" update
)
set "result=%errorlevel%"
echo.
pause
exit /b %result%

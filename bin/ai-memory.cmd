@echo off
setlocal
echo ai-memory: renamed to sessionmunch; forwarding >&2
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0sessionmunch.ps1" %*
exit /b %ERRORLEVEL%

@echo off
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0sessionmunch.ps1" %*
exit /b %ERRORLEVEL%

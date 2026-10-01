. "$PSScriptRoot\..\lib\sessionmunch-hook.ps1"
Invoke-SessionMunchHook -Event "pre-tool-use" -Agent "gemini-cli"
exit 0

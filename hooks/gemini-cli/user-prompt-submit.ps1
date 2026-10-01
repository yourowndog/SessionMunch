. "$PSScriptRoot\..\lib\sessionmunch-hook.ps1"
Invoke-SessionMunchHook -Event "user-prompt" -Agent "gemini-cli"
exit 0

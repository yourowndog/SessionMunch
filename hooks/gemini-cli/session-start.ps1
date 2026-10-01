. "$PSScriptRoot\..\lib\sessionmunch-hook.ps1"
Invoke-SessionMunchHook -Event "session-start" -Agent "gemini-cli" -FetchHandoff
exit 0

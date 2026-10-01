. "$PSScriptRoot\..\lib\sessionmunch-hook.ps1"
Invoke-SessionMunchHook -Event "session-start" -Agent "codex" -FetchHandoff
exit 0

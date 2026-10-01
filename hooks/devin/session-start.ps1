. "$PSScriptRoot\..\lib\sessionmunch-hook.ps1"
Invoke-SessionMunchHook -Event "session-start" -Agent "devin" -FetchHandoff
exit 0

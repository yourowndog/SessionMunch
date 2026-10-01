. "$PSScriptRoot\..\lib\sessionmunch-hook.ps1"
Invoke-SessionMunchHook -Event "session-start" -Agent "command-code" -FetchHandoff
exit 0

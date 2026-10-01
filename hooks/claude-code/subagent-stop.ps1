. "$PSScriptRoot\..\lib\sessionmunch-hook.ps1"
Invoke-SessionMunchHook -Event "subagent-stop" -Agent "claude-code"
exit 0

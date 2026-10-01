. "$PSScriptRoot\..\lib\sessionmunch-hook.ps1"
Invoke-SessionMunchHook -Event "session-start" -Agent "antigravity-cli" -FetchHandoff -AntigravityPreInvocationOutput
exit 0

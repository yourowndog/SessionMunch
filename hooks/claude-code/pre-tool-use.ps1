. "$PSScriptRoot\..\lib\sessionmunch-hook.ps1"
Invoke-SessionMunchHook -Event "pre-tool-use" -Agent "claude-code"
exit 0

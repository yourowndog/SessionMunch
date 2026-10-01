. "$PSScriptRoot\..\lib\sessionmunch-hook.ps1"
Invoke-SessionMunchHook -Event "pre-tool-use" -Agent "antigravity-cli"
[Console]::Out.WriteLine('{ "decision": "allow" }')
exit 0

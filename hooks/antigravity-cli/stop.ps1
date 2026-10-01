. "$PSScriptRoot\..\lib\sessionmunch-hook.ps1"
Invoke-SessionMunchHook -Event "stop" -Agent "antigravity-cli"
[Console]::Out.WriteLine('{"decision":""}')
exit 0

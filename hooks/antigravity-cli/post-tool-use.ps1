. "$PSScriptRoot\..\lib\sessionmunch-hook.ps1"
Invoke-SessionMunchHook -Event "post-tool-use" -Agent "antigravity-cli"
[Console]::Out.WriteLine("{}")
exit 0

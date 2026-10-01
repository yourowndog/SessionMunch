. "$PSScriptRoot\..\lib\sessionmunch-hook.ps1"
Invoke-SessionMunchHook -Event "post-tool-use" -Agent "grok"
exit 0

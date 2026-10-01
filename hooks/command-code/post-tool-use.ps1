. "$PSScriptRoot\..\lib\sessionmunch-hook.ps1"
Invoke-SessionMunchHook -Event "post-tool-use" -Agent "command-code"
exit 0

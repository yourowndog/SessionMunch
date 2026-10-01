. "$PSScriptRoot\..\lib\sessionmunch-hook.ps1"
Invoke-SessionMunchHook -Event "post-tool-use" -Agent "gemini-cli"
exit 0

# Kiro CLI v2 post-tool-use hook (postToolUse).
. "$PSScriptRoot\..\lib\sessionmunch-hook.ps1"
Invoke-SessionMunchHook -Event "post-tool-use" -Agent "kiro-cli"
exit 0

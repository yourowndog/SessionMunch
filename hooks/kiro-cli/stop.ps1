# Kiro CLI v2 stop hook (stop).
# Print nothing: on the v2 engine a stop hook's stdout can carry a
# {"decision":"block"} verdict that would re-prompt the model, so a
# capture-only hook keeps stdout empty and exits 0.
. "$PSScriptRoot\..\lib\sessionmunch-hook.ps1"
Invoke-SessionMunchHook -Event "stop" -Agent "kiro-cli"
exit 0

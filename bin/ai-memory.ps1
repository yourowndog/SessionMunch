# Deprecated compatibility shim (namespace migration t_3f5184b0).
# `ai-memory` is now `sessionmunch`. Forwards to the sibling
# `sessionmunch.ps1` so pre-rename hook scripts keep working until they
# are reinstalled. New code must call `sessionmunch`.
[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CommandArgs
)
$ErrorActionPreference = "Stop"
Write-Warning "ai-memory: renamed to sessionmunch; forwarding (reinstall wrappers to silence this)"
& "$PSScriptRoot/sessionmunch.ps1" @CommandArgs
exit $LASTEXITCODE

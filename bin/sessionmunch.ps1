# sessionmunch.ps1 - Windows PowerShell wrapper for the Docker image.
#
# This mirrors bin/sessionmunch for Windows users who run Docker Desktop.
# It forwards CLI commands into the Linux container with the user's home
# directory mounted at /host-home and the current project mounted at /work.
#
# The wrapper tells the Linux container to render Windows PowerShell hook
# commands that point at the host's staged .ps1 scripts.
[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CommandArgs
)

$ErrorActionPreference = "Stop"

function Get-EnvOrDefault {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Default
    )
    $value = [Environment]::GetEnvironmentVariable($Name)
    if ([string]::IsNullOrWhiteSpace($value)) {
        return $Default
    }
    return $value
}

# Resolve the real subcommand, skipping the global options that may precede
# it. `--config` and `--data-dir` take a separate value, so their argument has
# to be stepped over as well or it would be mistaken for the subcommand.
function Get-WrapperSubcommand {
    param([string[]]$WrapperArgs)

    $Index = 0
    while ($Index -lt $WrapperArgs.Count) {
        $Arg = $WrapperArgs[$Index]
        if ($Arg -eq "--config" -or $Arg -eq "--data-dir") {
            $Index += 2
        } elseif ($Arg -like "--*") {
            $Index += 1
        } else {
            return $Arg
        }
    }
    return ""
}

$Image = Get-EnvOrDefault "SESSIONMUNCH_IMAGE" "yourowndog/sessionmunch:latest"
$Docker = Get-EnvOrDefault "SESSIONMUNCH_DOCKER" "docker"
$DataVolume = Get-EnvOrDefault "SESSIONMUNCH_DATA_VOLUME" "sessionmunch-data"

if (-not (Get-Command $Docker -ErrorAction SilentlyContinue)) {
    Write-Error "Could not find Docker command '$Docker'. Install Docker Desktop or set SESSIONMUNCH_DOCKER."
    exit 127
}

if ($CommandArgs.Count -gt 0 -and $CommandArgs[0] -eq "upgrade") {
    & $Docker pull $Image
    exit $LASTEXITCODE
}

$HomePath = (Resolve-Path -LiteralPath $HOME).Path
$WorkPath = (Get-Location).Path
$HookHostRoot = ($HomePath -replace '\\', '/') + "/.local/share/sessionmunch/hooks"

$HomeRoot = $HomePath.TrimEnd([char[]]@('/', '\'))
$HomePrefix = $HomeRoot + [IO.Path]::DirectorySeparatorChar
$InsideHome = $WorkPath.Equals($HomeRoot, [StringComparison]::OrdinalIgnoreCase) -or
    $WorkPath.StartsWith($HomePrefix, [StringComparison]::OrdinalIgnoreCase)
$ScopeMountArgs = @()
if ($InsideHome) {
    $ScopeSuffix = if ($WorkPath.Length -eq $HomeRoot.Length) {
        ""
    } else {
        $WorkPath.Substring($HomeRoot.Length) -replace '\\', '/'
    }
    $ScopeCwd = "/host-home$ScopeSuffix"
} else {
    $ScopeRoot = $WorkPath
    if (Get-Command git -ErrorAction SilentlyContinue) {
        # Probe for the repo root, but never let git's "not a git
        # repository" message abort the wrapper. Under this script's
        # $ErrorActionPreference = 'Stop', Windows PowerShell 5.1 turns a
        # REDIRECTED native stderr (the 2>$null below) into a *terminating*
        # error, so running `status` outside a repo crashed the wrapper
        # (#591). Suppress locally, gate on the exit code, and restore. The
        # unredirected `& $Docker` calls are unaffected, which is why only
        # this probe tripped it.
        $PrevErrorAction = $ErrorActionPreference
        $ErrorActionPreference = 'SilentlyContinue'
        try {
            $DetectedScopeRoot = (& git -C $WorkPath rev-parse --show-toplevel 2>$null)
            if ($LASTEXITCODE -eq 0 -and -not [string]::IsNullOrWhiteSpace($DetectedScopeRoot)) {
                $ScopeRoot = [IO.Path]::GetFullPath($DetectedScopeRoot.Trim())
            }
        } finally {
            $ErrorActionPreference = $PrevErrorAction
        }
    }
    $ScopeRoot = $ScopeRoot.TrimEnd([char[]]@('/', '\'))
    $ScopePrefix = $ScopeRoot + [IO.Path]::DirectorySeparatorChar
    if ($WorkPath.Equals($ScopeRoot, [StringComparison]::OrdinalIgnoreCase)) {
        $ScopeSuffix = ""
    } elseif ($WorkPath.StartsWith($ScopePrefix, [StringComparison]::OrdinalIgnoreCase)) {
        $ScopeSuffix = $WorkPath.Substring($ScopeRoot.Length) -replace '\\', '/'
    } else {
        $ScopeRoot = $WorkPath
        $ScopeSuffix = ""
    }
    $ScopeMountArgs = @("-v", "${ScopeRoot}:/scope:ro")
    $ScopeCwd = "/scope$ScopeSuffix"
}

$DockerArgs = @("run", "--rm", "-i")
# Keep stdin attached in every mode. `SESSIONMUNCH_NO_TTY` suppresses only the
# pseudo-terminal allocation.
if (-not $env:SESSIONMUNCH_NO_TTY -and -not [Console]::IsInputRedirected -and -not [Console]::IsOutputRedirected) {
    $DockerArgs += "-t"
}

$DockerArgs += @(
    "-v", "${HomePath}:/host-home",
    "-v", "${WorkPath}:/work",
    "-w", "/work",
    "-e", "HOME=/host-home",
    "-e", "SESSIONMUNCH_HOST_CWD=$WorkPath",
    "-e", "SESSIONMUNCH_SCOPE_CWD=$ScopeCwd",
    "-e", "SESSIONMUNCH_DATA_DIR=/data",
    "-e", "SESSIONMUNCH_HOOK_PLATFORM=windows",
    "-e", "SESSIONMUNCH_HOOKS_HOST_ROOT=$HookHostRoot"
)
$DockerArgs += $ScopeMountArgs

if ($env:SESSIONMUNCH_DATA_DIR -and (Test-Path -LiteralPath $env:SESSIONMUNCH_DATA_DIR -PathType Container)) {
    $DataPath = (Resolve-Path -LiteralPath $env:SESSIONMUNCH_DATA_DIR).Path
    $DockerArgs += @("-v", "${DataPath}:/data")
} else {
    $DockerArgs += @("-v", "${DataVolume}:/data")
}

foreach ($Name in @(
    "SESSIONMUNCH_SERVER_URL",
    "SESSIONMUNCH_AUTH_TOKEN",
    "SESSIONMUNCH_LLM_PROVIDER",
    "SESSIONMUNCH_LLM_MODEL",
    "SESSIONMUNCH_LLM_BASE_URL",
    "SESSIONMUNCH_EMBEDDING_PROVIDER",
    "SESSIONMUNCH_EMBEDDING_MODEL",
    "SESSIONMUNCH_EMBEDDING_BASE_URL",
    "SESSIONMUNCH_EMBEDDING_DIM",
    "SESSIONMUNCH_ALLOWED_HOSTS",
    "CLAUDE_CODE_SESSION_ID",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "OPENAI_API_KEY",
    "VOYAGE_API_KEY",
    "LLM_API_KEY",
    "EMBEDDING_API_KEY",
    "RUST_LOG"
)) {
    if (-not [string]::IsNullOrEmpty([Environment]::GetEnvironmentVariable($Name))) {
        $DockerArgs += @("-e", $Name)
    }
}

# Docker Desktop gives Windows no host networking for Linux containers, so a
# thin-client command (status, search, bootstrap, ...) reaches the loopback-
# published server from this helper container through Docker Desktop's host
# alias; 127.0.0.1 would mean the helper container itself and the call dies
# with "Connection refused". But install-mcp/install-hooks/setup-agent RENDER
# the URL into the *host* agent config, and host.docker.internal does NOT
# resolve on the Windows host: baking it in silently breaks MCP and every
# capture hook. So for those commands leave SESSIONMUNCH_SERVER_URL unset, letting
# the CLI render its host-reachable default (http://127.0.0.1:49374).
# (issue #107)
$RendersHostConfig = (Get-WrapperSubcommand $CommandArgs) -in @(
    "install-mcp",
    "install-hooks",
    "setup-agent"
)
if (-not $RendersHostConfig -and
    [string]::IsNullOrEmpty([Environment]::GetEnvironmentVariable("SESSIONMUNCH_SERVER_URL"))) {
    $DockerArgs += @("-e", "SESSIONMUNCH_SERVER_URL=http://host.docker.internal:49374")
}

$DockerArgs += $Image
$DockerArgs += $CommandArgs

& $Docker @DockerArgs
exit $LASTEXITCODE

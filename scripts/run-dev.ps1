[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$ConfigPath
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = Split-Path -Parent $PSScriptRoot
$webRoot = Join-Path $repositoryRoot 'web'
$nodeModules = Join-Path $webRoot 'node_modules'

if (-not (Test-Path -LiteralPath $ConfigPath -PathType Leaf)) {
    throw 'The Cellar config file does not exist.'
}
$resolvedConfig = (Resolve-Path -LiteralPath $ConfigPath).Path

if (-not (Test-Path -LiteralPath $nodeModules -PathType Container)) {
    throw 'web/node_modules is missing. Install the frontend dependencies before running Cellar.'
}

$previousConfig = [Environment]::GetEnvironmentVariable('CELLAR_CONFIG', 'Process')
try {
    Push-Location $webRoot
    try {
        & npm run build
        if ($LASTEXITCODE -ne 0) {
            throw 'The frontend build failed.'
        }
    }
    finally {
        Pop-Location
    }

    [Environment]::SetEnvironmentVariable('CELLAR_CONFIG', $resolvedConfig, 'Process')
    Push-Location $repositoryRoot
    try {
        & cargo run
        $serverExitCode = $LASTEXITCODE
    }
    finally {
        Pop-Location
    }
}
finally {
    [Environment]::SetEnvironmentVariable('CELLAR_CONFIG', $previousConfig, 'Process')
}

exit $serverExitCode

[CmdletBinding()]
param(
    [ValidateRange(1024, 65535)]
    [int]$Port = 8788,

    [string]$DataRoot = 'D:\Cellar-dev\data',

    [string]$Password = $env:CELLAR_DEV_PASSWORD,

    [ValidatePattern('^[a-z0-9](?:[a-z0-9-]{0,50}[a-z0-9])?$')]
    [string]$VercelProject = 'cellar-entry',

    [string]$VercelStatePath = 'D:\Cellar\config\vercel-routes.json',

    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = $PSScriptRoot
$webRoot = Join-Path $repositoryRoot 'web'
$webDistRoot = Join-Path $webRoot 'dist'
$webDistPlaceholder = Join-Path $webDistRoot '.gitkeep'
$serverExitCode = 1

if (-not [IO.Path]::IsPathRooted($DataRoot)) {
    throw 'DataRoot must be an absolute path.'
}
if (-not [IO.Path]::IsPathRooted($VercelStatePath)) {
    throw 'VercelStatePath must be an absolute path.'
}
if ($Port -eq 8787) {
    throw 'Port 8787 is reserved for production. Use the development default, 8788.'
}

try {
    if (-not $SkipBuild) {
        $npm = Get-Command 'npm.cmd' -ErrorAction SilentlyContinue
        if ($null -eq $npm) {
            throw 'Node.js and npm are required to build the frontend.'
        }
        if (-not (Test-Path -LiteralPath (Join-Path $webRoot 'node_modules') -PathType Container)) {
            & $npm.Source --prefix $webRoot ci
            if ($LASTEXITCODE -ne 0) {
                throw 'The frontend dependency installation failed.'
            }
        }
        & $npm.Source --prefix $webRoot run build
        if ($LASTEXITCODE -ne 0) {
            throw 'The frontend build failed.'
        }
        & cargo build --manifest-path (Join-Path $repositoryRoot 'Cargo.toml') --release
        if ($LASTEXITCODE -ne 0) {
            throw 'The Cellar development build failed.'
        }
    }

    & (Join-Path $repositoryRoot 'cellar.ps1') `
        -Port $Port `
        -DataRoot $DataRoot `
        -Password $Password `
        -VercelProject $VercelProject `
        -VercelPath '/dev' `
        -VercelStatePath $VercelStatePath
    $serverExitCode = $LASTEXITCODE
}
finally {
    if (-not (Test-Path -LiteralPath $webDistRoot -PathType Container)) {
        [IO.Directory]::CreateDirectory($webDistRoot) | Out-Null
    }
    [IO.File]::WriteAllText($webDistPlaceholder, "`n", [Text.UTF8Encoding]::new($false))
}

exit $serverExitCode

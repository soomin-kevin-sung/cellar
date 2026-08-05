[CmdletBinding()]
param(
    [string]$Destination = 'D:\Cellar',

    [ValidateRange(1024, 65535)]
    [int]$Port = 8787,

    [ValidatePattern('^[a-z0-9](?:[a-z0-9-]{0,50}[a-z0-9])?$')]
    [string]$VercelProject = 'cellar-entry',

    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = $PSScriptRoot

if (-not [IO.Path]::IsPathRooted($Destination)) {
    throw 'Destination must be an absolute path.'
}

$resolvedDestination = [IO.Path]::GetFullPath($Destination).TrimEnd('\')
$destinationRoot = [IO.Path]::GetPathRoot($resolvedDestination).TrimEnd('\')
if ($resolvedDestination -eq $destinationRoot) {
    throw 'Destination cannot be the root of a drive.'
}

if (-not $SkipBuild) {
    $npm = Get-Command 'npm.cmd' -ErrorAction SilentlyContinue
    if ($null -eq $npm) {
        throw 'Node.js and npm are required to build the frontend.'
    }

    & $npm.Source --prefix (Join-Path $repositoryRoot 'web') ci
    if ($LASTEXITCODE -ne 0) {
        throw 'The frontend dependency installation failed.'
    }

    & $npm.Source --prefix (Join-Path $repositoryRoot 'web') run build
    if ($LASTEXITCODE -ne 0) {
        throw 'The frontend build failed.'
    }

    & cargo build --manifest-path (Join-Path $repositoryRoot 'Cargo.toml') --release
    if ($LASTEXITCODE -ne 0) {
        throw 'The Cellar release build failed.'
    }
}

$releaseExe = Join-Path $repositoryRoot 'target\release\cellar.exe'
if (-not (Test-Path -LiteralPath $releaseExe -PathType Leaf)) {
    throw 'The release executable is missing. Run deploy.ps1 without -SkipBuild first.'
}

$appRoot = Join-Path $resolvedDestination 'app'
$binRoot = Join-Path $resolvedDestination 'bin'
$configRoot = Join-Path $resolvedDestination 'config'
$dataRoot = Join-Path $resolvedDestination 'data'
$logsRoot = Join-Path $resolvedDestination 'logs'

foreach ($directory in @($resolvedDestination, $appRoot, $binRoot, $configRoot, $dataRoot, $logsRoot)) {
    [IO.Directory]::CreateDirectory($directory) | Out-Null
}

try {
    Copy-Item -LiteralPath $releaseExe `
        -Destination (Join-Path $appRoot 'cellar.exe') `
        -Force
}
catch {
    throw 'Could not update cellar.exe. Stop the installed Cellar server and deploy again.'
}

Copy-Item -LiteralPath (Join-Path $repositoryRoot 'cellar.ps1') `
    -Destination (Join-Path $resolvedDestination 'cellar.ps1') `
    -Force

$cloudflaredCommand = Get-Command 'cloudflared.exe' -ErrorAction SilentlyContinue
$managedCloudflared = Join-Path $env:LOCALAPPDATA 'Cellar\bin\cloudflared.exe'
$cloudflaredSource = if ($null -ne $cloudflaredCommand) {
    $cloudflaredCommand.Source
}
elseif (Test-Path -LiteralPath $managedCloudflared -PathType Leaf) {
    $managedCloudflared
}
else {
    $null
}

if ($null -ne $cloudflaredSource) {
    Copy-Item -LiteralPath $cloudflaredSource `
        -Destination (Join-Path $binRoot 'cloudflared.exe') `
        -Force
}

$launcherConfig = [ordered]@{
    port = $Port
    vercelProject = $VercelProject
    vercelPath = '/'
    vercelStatePath = (Join-Path $configRoot 'vercel-routes.json')
} | ConvertTo-Json
[IO.File]::WriteAllText(
    (Join-Path $configRoot 'launcher.json'),
    $launcherConfig,
    [Text.UTF8Encoding]::new($false)
)

$deployment = [ordered]@{
    deployedAt = [DateTimeOffset]::Now.ToString('o')
    source = $repositoryRoot
    executableSha256 = (Get-FileHash -LiteralPath $releaseExe -Algorithm SHA256).Hash.ToLowerInvariant()
} | ConvertTo-Json
[IO.File]::WriteAllText(
    (Join-Path $appRoot 'deployment.json'),
    $deployment,
    [Text.UTF8Encoding]::new($false)
)

Write-Host "Cellar deployed: $resolvedDestination" -ForegroundColor Green
Write-Host "Start: & '$resolvedDestination\cellar.ps1' -Password '<at-least-16-characters>'"

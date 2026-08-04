[CmdletBinding()]
param(
    [string]$InstallRoot = 'D:\Cellar',

    [ValidateRange(1024, 65535)]
    [int]$Port = 8787,

    [ValidatePattern('^[a-z0-9](?:[a-z0-9-]{0,50}[a-z0-9])?$')]
    [string]$VercelProject = 'cellar-entry',

    [switch]$SkipBuild
)

$deployScript = Join-Path $PSScriptRoot 'deploy.ps1'
try {
    & $deployScript `
        -Destination $InstallRoot `
        -Port $Port `
        -VercelProject $VercelProject `
        -SkipBuild:$SkipBuild
}
catch {
    Write-Error $_
    exit 1
}
exit 0

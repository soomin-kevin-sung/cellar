[CmdletBinding()]
param(
    [string]$Password = $env:CELLAR_DEV_PASSWORD
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = $PSScriptRoot
$webRoot = Join-Path $repositoryRoot 'web'
$dataRoot = 'D:\Cellar-dev\data'
$logsRoot = 'D:\Cellar-dev\logs'
$backendPort = 8788
$webPort = 4173
$viteProcess = $null

function Stop-ProcessTree {
    param([int]$RootId)

    $processes = Get-CimInstance Win32_Process -ErrorAction SilentlyContinue
    $pending = @($RootId)
    $ids = [Collections.Generic.List[int]]::new()
    while ($pending.Count -gt 0) {
        $parentId = $pending[0]
        $pending = @($pending | Select-Object -Skip 1)
        $children = @($processes | Where-Object { $_.ParentProcessId -eq $parentId })
        foreach ($child in $children) {
            $pending += [int]$child.ProcessId
        }
        $ids.Add($parentId)
    }
    foreach ($id in @($ids | Sort-Object -Descending)) {
        Stop-Process -Id $id -Force -ErrorAction SilentlyContinue
    }
}

$npm = Get-Command 'npm.cmd' -ErrorAction SilentlyContinue
if ($null -eq $npm) {
    throw 'Node.js and npm are required.'
}
if (-not (Test-Path -LiteralPath (Join-Path $webRoot 'node_modules') -PathType Container)) {
    & $npm.Source --prefix $webRoot ci
    if ($LASTEXITCODE -ne 0) {
        throw 'The frontend dependency installation failed.'
    }
}

& cargo build --manifest-path (Join-Path $repositoryRoot 'Cargo.toml') --release
if ($LASTEXITCODE -ne 0) {
    throw 'The Cellar backend build failed.'
}

[IO.Directory]::CreateDirectory($logsRoot) | Out-Null
$viteOut = Join-Path $logsRoot 'vite.out.log'
$viteErr = Join-Path $logsRoot 'vite.err.log'

try {
    $viteProcess = Start-Process -FilePath $npm.Source `
        -ArgumentList @('run', 'dev') `
        -WorkingDirectory $webRoot `
        -RedirectStandardOutput $viteOut `
        -RedirectStandardError $viteErr `
        -WindowStyle Hidden `
        -PassThru

    $viteReady = $false
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    while ([DateTime]::UtcNow -lt $deadline -and -not $viteReady) {
        if ($viteProcess.HasExited) {
            $details = if (Test-Path -LiteralPath $viteErr) {
                Get-Content -LiteralPath $viteErr -Raw
            }
            else { '' }
            throw "Vite failed to start. $details"
        }
        try {
            $response = Invoke-WebRequest `
                -Uri "http://127.0.0.1:$webPort/" `
                -UseBasicParsing `
                -TimeoutSec 1
            $viteReady = $response.StatusCode -eq 200
        }
        catch {
            Start-Sleep -Milliseconds 250
        }
    }
    if (-not $viteReady) {
        throw 'Vite did not become ready within 20 seconds.'
    }

    & (Join-Path $repositoryRoot 'scripts\start-cellar.ps1') `
        -Port $backendPort `
        -TunnelPort $webPort `
        -DataRoot $dataRoot `
        -Password $Password `
        -VercelProject 'cellar-entry' `
        -VercelPath '/dev' `
        -VercelStatePath 'D:\Cellar\config\vercel-routes.json'
    $serverExitCode = $LASTEXITCODE
}
finally {
    if ($null -ne $viteProcess -and -not $viteProcess.HasExited) {
        Stop-ProcessTree -RootId $viteProcess.Id
    }
}

exit $serverExitCode

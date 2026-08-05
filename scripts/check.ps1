[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repositoryRoot = Split-Path -Parent $PSScriptRoot
$webRoot = Join-Path $repositoryRoot 'web'
$webDistRoot = Join-Path $webRoot 'dist'
$webDistPlaceholder = Join-Path $webDistRoot '.gitkeep'
$startingLocation = Get-Location
$scriptExitCode = 0

function Assert-CommandSucceeded {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Command,

        [Parameter(Mandatory = $true)]
        [int]$ExitCode
    )

    if ($ExitCode -ne 0) {
        throw "Command '$Command' exited with code $ExitCode."
    }
}

try {
    Set-Location -LiteralPath $repositoryRoot
    & cargo fmt --check
    Assert-CommandSucceeded -Command 'cargo fmt --check' -ExitCode $LASTEXITCODE

    & cargo test
    Assert-CommandSucceeded -Command 'cargo test' -ExitCode $LASTEXITCODE

    & cargo clippy --all-targets -- -D warnings
    Assert-CommandSucceeded -Command 'cargo clippy --all-targets -- -D warnings' -ExitCode $LASTEXITCODE

    Push-Location -LiteralPath $webRoot
    try {
        & npm test
        Assert-CommandSucceeded -Command 'npm test' -ExitCode $LASTEXITCODE

        & npm run typecheck
        Assert-CommandSucceeded -Command 'npm run typecheck' -ExitCode $LASTEXITCODE

        & npm run lint
        Assert-CommandSucceeded -Command 'npm run lint' -ExitCode $LASTEXITCODE

        & npm run build
        Assert-CommandSucceeded -Command 'npm run build' -ExitCode $LASTEXITCODE
    }
    finally {
        Pop-Location
    }

    & cargo build --release
    Assert-CommandSucceeded -Command 'cargo build --release' -ExitCode $LASTEXITCODE
}
catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    $scriptExitCode = 1
}
finally {
    try {
        if (-not (Test-Path -LiteralPath $webDistRoot -PathType Container)) {
            [System.IO.Directory]::CreateDirectory($webDistRoot) | Out-Null
        }
        $webDistItem = Get-Item -LiteralPath $webDistRoot -Force
        if (($webDistItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw 'Refusing to write through a web/dist reparse point.'
        }
        if (Test-Path -LiteralPath $webDistPlaceholder) {
            $placeholderItem = Get-Item -LiteralPath $webDistPlaceholder -Force
            if ($placeholderItem.PSIsContainer -or
                ($placeholderItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw 'Refusing to replace an unsafe web/dist/.gitkeep entry.'
            }
        }
        [System.IO.File]::WriteAllText(
            $webDistPlaceholder,
            "`n",
            [System.Text.UTF8Encoding]::new($false)
        )
    }
    catch {
        [Console]::Error.WriteLine("Failed to restore web/dist/.gitkeep: $($_.Exception.Message)")
        $scriptExitCode = 1
    }

    try {
        Set-Location -LiteralPath $startingLocation.Path
    }
    catch {
        [Console]::Error.WriteLine("Failed to restore the starting location: $($_.Exception.Message)")
        $scriptExitCode = 1
    }
}

exit $scriptExitCode

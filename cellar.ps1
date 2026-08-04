[CmdletBinding()]
param(
    [ValidateRange(1024, 65535)]
    [int]$Port = 8787,

    [string]$DataRoot = (Join-Path $env:LOCALAPPDATA 'Cellar\data'),

    [string]$Password,

    [ValidatePattern('^[a-z0-9](?:[a-z0-9-]{0,50}[a-z0-9])?$')]
    [string]$VercelProject = $env:CELLAR_VERCEL_PROJECT
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = $PSScriptRoot
$cellarExe = Join-Path $repositoryRoot 'target\release\cellar.exe'
$managedBinRoot = Join-Path $env:LOCALAPPDATA 'Cellar\bin'
$managedCloudflared = Join-Path $managedBinRoot 'cloudflared.exe'
$cloudflaredVersion = '2026.7.3'
$cloudflaredSha256 = '8635da433b6df8194746e88ed9d2589566c20e38bfc2a80e431a348b7c765841'

function Find-Cloudflared {
    $command = Get-Command 'cloudflared.exe' -ErrorAction SilentlyContinue
    if ($null -ne $command) {
        return $command.Source
    }

    if (Test-Path -LiteralPath $managedCloudflared -PathType Leaf) {
        return $managedCloudflared
    }

    return $null
}

function Publish-VercelEntry {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Project,

        [Parameter(Mandatory = $true)]
        [string]$Destination,

        [Parameter(Mandatory = $true)]
        [string]$WorkingDirectory
    )

    $npx = Get-Command 'npx.cmd' -ErrorAction SilentlyContinue
    if ($null -eq $npx) {
        throw 'Vercel 진입 주소를 배포하려면 Node.js와 npx가 필요합니다.'
    }

    $entryRoot = Join-Path $WorkingDirectory 'vercel-entry'
    [IO.Directory]::CreateDirectory($entryRoot) | Out-Null
    $vercelConfig = @{
        '$schema' = 'https://openapi.vercel.sh/vercel.json'
        redirects = @(
            @{
                source = '/(.*)'
                destination = $Destination
                permanent = $false
            }
        )
    } | ConvertTo-Json -Depth 5
    [IO.File]::WriteAllText(
        (Join-Path $entryRoot 'vercel.json'),
        $vercelConfig,
        [Text.UTF8Encoding]::new($false)
    )

    $output = & $npx.Source --yes vercel deploy $entryRoot --prod --yes --project $Project --no-color 2>&1
    $outputText = ($output | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine
    if ($LASTEXITCODE -ne 0) {
        throw "Vercel 배포에 실패했습니다. 먼저 'npx vercel login'을 실행해 로그인하세요.`n$outputText"
    }

    $urls = [regex]::Matches($outputText, 'https://[a-z0-9.-]+\.vercel\.app')
    if ($urls.Count -gt 0) {
        return $urls[$urls.Count - 1].Value
    }
    return "https://$Project.vercel.app"
}

if (-not (Test-Path -LiteralPath $cellarExe -PathType Leaf)) {
    throw 'Cellar 실행 파일이 없습니다. 먼저 cargo build --release를 실행하세요.'
}

$cloudflaredExe = Find-Cloudflared
if ($null -eq $cloudflaredExe) {
    Write-Host 'cloudflared를 사용자 폴더에 처음 한 번 설치합니다...'
    [IO.Directory]::CreateDirectory($managedBinRoot) | Out-Null
    $downloadPath = "$managedCloudflared.download"
    $downloadUrl = "https://github.com/cloudflare/cloudflared/releases/download/$cloudflaredVersion/cloudflared-windows-amd64.exe"
    try {
        Invoke-WebRequest -Uri $downloadUrl -OutFile $downloadPath -UseBasicParsing
        $actualSha256 = (Get-FileHash -LiteralPath $downloadPath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actualSha256 -ne $cloudflaredSha256) {
            throw '다운로드한 cloudflared의 무결성 검증에 실패했습니다.'
        }
        Move-Item -LiteralPath $downloadPath -Destination $managedCloudflared -Force
    }
    finally {
        if (Test-Path -LiteralPath $downloadPath -PathType Leaf) {
            Remove-Item -LiteralPath $downloadPath -Force -ErrorAction SilentlyContinue
        }
    }
    $cloudflaredExe = $managedCloudflared
}

$resolvedDataRoot = [IO.Path]::GetFullPath($DataRoot)
[IO.Directory]::CreateDirectory($resolvedDataRoot) | Out-Null
$cellarDirectory = Join-Path $resolvedDataRoot '.cellar'
[IO.Directory]::CreateDirectory($cellarDirectory) | Out-Null

$runtimeRoot = Join-Path ([IO.Path]::GetTempPath()) ('cellar-' + [guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($runtimeRoot) | Out-Null
$tunnelOut = Join-Path $runtimeRoot 'cloudflared.out.log'
$tunnelErr = Join-Path $runtimeRoot 'cloudflared.err.log'
$runtimeConfig = Join-Path $runtimeRoot 'config.toml'
$tunnelProcess = $null
$previousConfig = [Environment]::GetEnvironmentVariable('CELLAR_CONFIG', 'Process')
$previousPassword = [Environment]::GetEnvironmentVariable('CELLAR_QUICK_PASSWORD', 'Process')

try {
    $tunnelProcess = Start-Process -FilePath $cloudflaredExe `
        -ArgumentList @('tunnel', '--url', "http://127.0.0.1:$Port", '--no-autoupdate') `
        -RedirectStandardOutput $tunnelOut `
        -RedirectStandardError $tunnelErr `
        -WindowStyle Hidden `
        -PassThru

    $publicUrl = $null
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    while ([DateTime]::UtcNow -lt $deadline -and $null -eq $publicUrl) {
        if ($tunnelProcess.HasExited) {
            $details = if (Test-Path -LiteralPath $tunnelErr) { Get-Content -LiteralPath $tunnelErr -Raw } else { '' }
            throw "Cloudflare 임시 터널 시작에 실패했습니다. $details"
        }
        Start-Sleep -Milliseconds 250
        $logs = ''
        if (Test-Path -LiteralPath $tunnelOut) { $logs += Get-Content -LiteralPath $tunnelOut -Raw }
        if (Test-Path -LiteralPath $tunnelErr) { $logs += Get-Content -LiteralPath $tunnelErr -Raw }
        $match = [regex]::Match($logs, 'https://[a-z0-9-]+\.trycloudflare\.com')
        if ($match.Success) {
            $publicUrl = $match.Value
        }
    }
    if ($null -eq $publicUrl) {
        throw '30초 안에 Cloudflare 임시 주소를 받지 못했습니다.'
    }

    $entryUrl = $null
    if (-not [string]::IsNullOrWhiteSpace($VercelProject)) {
        Write-Host 'Vercel 고정 진입 주소를 갱신하는 중...'
        try {
            $entryUrl = Publish-VercelEntry `
                -Project $VercelProject `
                -Destination $publicUrl `
                -WorkingDirectory $runtimeRoot
        }
        catch {
            Write-Warning $_.Exception.Message
        }
    }

    if ([string]::IsNullOrEmpty($Password)) {
        $passwordBytes = New-Object byte[] 18
        $random = [Security.Cryptography.RandomNumberGenerator]::Create()
        try {
            $random.GetBytes($passwordBytes)
        }
        finally {
            $random.Dispose()
        }
        $Password = [Convert]::ToBase64String($passwordBytes).TrimEnd('=').Replace('+', '-').Replace('/', '_')
    }
    elseif ($Password.Length -lt 16) {
        throw '지정한 임시 암호는 16자 이상이어야 합니다.'
    }
    $tomlDataRoot = $resolvedDataRoot.Replace('\', '/').Replace('"', '\"')
    $tomlDatabasePath = (Join-Path $cellarDirectory 'cellar.db').Replace('\', '/').Replace('"', '\"')
    $configText = @"
bind = "127.0.0.1:$Port"
external_origin = "$publicUrl"
data_root = "$tomlDataRoot"
database_path = "$tomlDatabasePath"

[access]
team_domain = "https://unused.cloudflareaccess.com"
audience = "unused-in-quick-tunnel-mode"
owner_email = "owner@localhost.invalid"
"@
    [IO.File]::WriteAllText($runtimeConfig, $configText, [Text.UTF8Encoding]::new($false))
    [Environment]::SetEnvironmentVariable('CELLAR_CONFIG', $runtimeConfig, 'Process')
    [Environment]::SetEnvironmentVariable('CELLAR_QUICK_PASSWORD', $Password, 'Process')

    Write-Host ''
    Write-Host 'Cellar가 외부에 연결되었습니다.' -ForegroundColor Green
    Write-Host "주소: $publicUrl"
    Write-Host '초기 관리자: cellar'
    Write-Host "초기 관리자 암호: $Password"
    Write-Host '관리자 계정이 이미 생성되었다면 기존 암호로 로그인하세요.'
    Write-Host '종료: Ctrl+C (다시 실행하면 임시 주소가 바뀝니다.)'
    Write-Host ''

    if ($null -ne $entryUrl) {
        Write-Host "고정 진입 주소: $entryUrl" -ForegroundColor Cyan
        Write-Host ''
    }

    & $cellarExe
    $cellarExitCode = $LASTEXITCODE
}
finally {
    [Environment]::SetEnvironmentVariable('CELLAR_CONFIG', $previousConfig, 'Process')
    [Environment]::SetEnvironmentVariable('CELLAR_QUICK_PASSWORD', $previousPassword, 'Process')
    if ($null -ne $tunnelProcess -and -not $tunnelProcess.HasExited) {
        Stop-Process -Id $tunnelProcess.Id -Force -ErrorAction SilentlyContinue
        $tunnelProcess.WaitForExit()
    }
    if (Test-Path -LiteralPath $runtimeRoot -PathType Container) {
        Remove-Item -LiteralPath $runtimeRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

exit $cellarExitCode

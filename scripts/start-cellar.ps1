[CmdletBinding()]
param(
    [ValidateRange(1024, 65535)]
    [int]$Port = 8787,

    [int]$TunnelPort,

    [string]$DataRoot,

    [string]$Password = $env:CELLAR_PASSWORD,

    [ValidatePattern('^[a-z0-9](?:[a-z0-9-]{0,50}[a-z0-9])?$')]
    [string]$VercelProject = $env:CELLAR_VERCEL_PROJECT,

    [ValidatePattern('^/(?:[a-z0-9-]+)?$')]
    [string]$VercelPath = $env:CELLAR_VERCEL_PATH,

    [string]$VercelStatePath = $env:CELLAR_VERCEL_STATE_PATH
)

$ErrorActionPreference = 'Stop'
$sourceRepositoryRoot = Split-Path -Parent $PSScriptRoot
$repositoryRoot = if (Test-Path -LiteralPath (Join-Path $sourceRepositoryRoot 'Cargo.toml') -PathType Leaf) {
    $sourceRepositoryRoot
}
else {
    $PSScriptRoot
}
$installedExe = Join-Path $repositoryRoot 'app\cellar.exe'
$developmentExe = Join-Path $repositoryRoot 'target\release\cellar.exe'
$installedLayout = Test-Path -LiteralPath $installedExe -PathType Leaf
$cellarExe = if ($installedLayout) { $installedExe } else { $developmentExe }
$launcherConfigPath = Join-Path $repositoryRoot 'config\launcher.json'

if (Test-Path -LiteralPath $launcherConfigPath -PathType Leaf) {
    try {
        $launcherConfig = Get-Content -LiteralPath $launcherConfigPath -Raw -Encoding utf8 |
            ConvertFrom-Json
    }
    catch {
        throw "Invalid launcher configuration: $launcherConfigPath"
    }

    if (-not $PSBoundParameters.ContainsKey('Port') -and
        $launcherConfig.PSObject.Properties.Name -contains 'port') {
        $configuredPort = [int]$launcherConfig.port
        if ($configuredPort -lt 1024 -or $configuredPort -gt 65535) {
            throw 'Configured port must be between 1024 and 65535.'
        }
        $Port = $configuredPort
    }

    if ([string]::IsNullOrWhiteSpace($VercelProject) -and
        $launcherConfig.PSObject.Properties.Name -contains 'vercelProject') {
        $VercelProject = [string]$launcherConfig.vercelProject
    }

    if ([string]::IsNullOrWhiteSpace($VercelPath) -and
        $launcherConfig.PSObject.Properties.Name -contains 'vercelPath') {
        $VercelPath = [string]$launcherConfig.vercelPath
    }

    if ([string]::IsNullOrWhiteSpace($VercelStatePath) -and
        $launcherConfig.PSObject.Properties.Name -contains 'vercelStatePath') {
        $VercelStatePath = [string]$launcherConfig.vercelStatePath
    }
}

if ([string]::IsNullOrWhiteSpace($VercelPath)) {
    $VercelPath = '/'
}

if (-not $PSBoundParameters.ContainsKey('TunnelPort')) {
    $TunnelPort = $Port
}
if ($TunnelPort -lt 1024 -or $TunnelPort -gt 65535) {
    throw 'TunnelPort must be between 1024 and 65535.'
}

if ([string]::IsNullOrWhiteSpace($VercelStatePath)) {
    $VercelStatePath = if ($installedLayout) {
        Join-Path $repositoryRoot 'config\vercel-routes.json'
    }
    else {
        Join-Path $env:LOCALAPPDATA 'Cellar\vercel-routes.json'
    }
}

if ([string]::IsNullOrWhiteSpace($DataRoot)) {
    $DataRoot = if ($installedLayout) {
        Join-Path $repositoryRoot 'data'
    }
    else {
        Join-Path $env:LOCALAPPDATA 'Cellar\data'
    }
}

$managedBinRoot = if ($installedLayout) {
    Join-Path $repositoryRoot 'bin'
}
else {
    Join-Path $env:LOCALAPPDATA 'Cellar\bin'
}
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
        [string]$RoutePath,

        [Parameter(Mandatory = $true)]
        [string]$StatePath,

        [Parameter(Mandatory = $true)]
        [string]$WorkingDirectory
    )

    $npx = Get-Command 'npx.cmd' -ErrorAction SilentlyContinue
    if ($null -eq $npx) {
        throw 'Vercel 진입 주소를 배포하려면 Node.js와 npx가 필요합니다.'
    }

    $destinationUri = $null
    if (-not [Uri]::TryCreate($Destination, [UriKind]::Absolute, [ref]$destinationUri) -or
        $destinationUri.Scheme -ne 'https' -or
        $destinationUri.AbsolutePath -ne '/' -or
        -not [string]::IsNullOrEmpty($destinationUri.Query) -or
        -not [string]::IsNullOrEmpty($destinationUri.Fragment)) {
        throw 'Vercel destination must be an HTTPS origin without a path, query, or fragment.'
    }
    $normalizedDestination = $destinationUri.GetLeftPart([UriPartial]::Authority)

    $resolvedStatePath = [IO.Path]::GetFullPath($StatePath)
    $stateDirectory = Split-Path -Parent $resolvedStatePath
    [IO.Directory]::CreateDirectory($stateDirectory) | Out-Null

    $routes = [ordered]@{}
    if (Test-Path -LiteralPath $resolvedStatePath -PathType Leaf) {
        try {
            $savedState = Get-Content -LiteralPath $resolvedStatePath -Raw -Encoding utf8 |
                ConvertFrom-Json
            if ($savedState.project -and [string]$savedState.project -ne $Project) {
                throw "The saved Vercel project is '$($savedState.project)', not '$Project'."
            }
            if ($savedState.routes) {
                foreach ($property in $savedState.routes.PSObject.Properties) {
                    $routes[$property.Name] = [string]$property.Value
                }
            }
        }
        catch {
            throw "Invalid Vercel route state: $resolvedStatePath. $($_.Exception.Message)"
        }
    }

    if ($RoutePath -ne '/' -and -not $routes.Contains('/')) {
        $request = [Net.HttpWebRequest]::Create("https://$Project.vercel.app/")
        $request.Method = 'HEAD'
        $request.AllowAutoRedirect = $false
        $response = $null
        try {
            $response = $request.GetResponse()
        }
        catch [Net.WebException] {
            $response = $_.Exception.Response
        }
        if ($null -ne $response) {
            try {
                $existingLocation = $response.Headers['Location']
                $existingUri = $null
                if ([Uri]::TryCreate($existingLocation, [UriKind]::Absolute, [ref]$existingUri) -and
                    $existingUri.Scheme -eq 'https') {
                    $routes['/'] = $existingUri.GetLeftPart([UriPartial]::Authority)
                }
            }
            finally {
                $response.Close()
            }
        }
    }

    $routes[$RoutePath] = $normalizedDestination
    if (-not $routes.Contains('/')) {
        throw 'The production Vercel route is missing. Start production once before publishing /dev.'
    }

    $redirects = [Collections.Generic.List[object]]::new()
    foreach ($path in @($routes.Keys | Where-Object { $_ -ne '/' } | Sort-Object Length -Descending)) {
        $target = [string]$routes[$path]
        $redirects.Add([ordered]@{
            source = $path
            destination = $target
            permanent = $false
        })
        $redirects.Add([ordered]@{
            source = "$path/:path*"
            destination = "$target/:path*"
            permanent = $false
        })
    }
    $redirects.Add([ordered]@{
        source = '/'
        destination = [string]$routes['/']
        permanent = $false
    })
    $redirects.Add([ordered]@{
        source = '/:path*'
        destination = "$($routes['/'])/:path*"
        permanent = $false
    })

    $entryRoot = Join-Path $WorkingDirectory 'vercel-entry'
    [IO.Directory]::CreateDirectory($entryRoot) | Out-Null
    $vercelConfig = [ordered]@{
        '$schema' = 'https://openapi.vercel.sh/vercel.json'
        redirects = $redirects
    } | ConvertTo-Json -Depth 5
    [IO.File]::WriteAllText(
        (Join-Path $entryRoot 'vercel.json'),
        $vercelConfig,
        [Text.UTF8Encoding]::new($false)
    )

    $previousErrorActionPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        $output = & $npx.Source --yes vercel deploy $entryRoot --prod --yes --project $Project --no-color 2>&1
        $vercelExitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    $outputText = ($output | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine
    if ($vercelExitCode -ne 0) {
        throw "Vercel 배포에 실패했습니다. 먼저 'npx vercel login'을 실행해 로그인하세요.`n$outputText"
    }

    $state = [ordered]@{
        schemaVersion = 1
        project = $Project
        routes = $routes
        updatedAt = [DateTimeOffset]::Now.ToString('o')
    } | ConvertTo-Json -Depth 5
    $temporaryStatePath = "$resolvedStatePath.tmp"
    [IO.File]::WriteAllText($temporaryStatePath, $state, [Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $temporaryStatePath -Destination $resolvedStatePath -Force

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
if ($installedLayout) {
    $configRoot = Join-Path $repositoryRoot 'config'
    $logsRoot = Join-Path $repositoryRoot 'logs'
    [IO.Directory]::CreateDirectory($configRoot) | Out-Null
    [IO.Directory]::CreateDirectory($logsRoot) | Out-Null
    $tunnelOut = Join-Path $logsRoot 'cloudflared.out.log'
    $tunnelErr = Join-Path $logsRoot 'cloudflared.err.log'
    $runtimeConfig = Join-Path $configRoot 'runtime.toml'
}
else {
    $tunnelOut = Join-Path $runtimeRoot 'cloudflared.out.log'
    $tunnelErr = Join-Path $runtimeRoot 'cloudflared.err.log'
    $runtimeConfig = Join-Path $runtimeRoot 'config.toml'
}
$tunnelProcess = $null
$previousConfig = [Environment]::GetEnvironmentVariable('CELLAR_CONFIG', 'Process')
$previousPassword = [Environment]::GetEnvironmentVariable('CELLAR_QUICK_PASSWORD', 'Process')

try {
    $tunnelProcess = Start-Process -FilePath $cloudflaredExe `
        -ArgumentList @('tunnel', '--url', "http://127.0.0.1:$TunnelPort", '--no-autoupdate') `
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
                -RoutePath $VercelPath `
                -StatePath $VercelStatePath `
                -WorkingDirectory $runtimeRoot
        }
        catch {
            Write-Warning $_.Exception.Message
        }
    }

    $passwordWasGenerated = [string]::IsNullOrEmpty($Password)
    if ($passwordWasGenerated) {
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
    if ($passwordWasGenerated) {
        Write-Host "초기 관리자 암호: $Password"
    }
    else {
        Write-Host 'Initial admin password: provided by the execution environment'
    }
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

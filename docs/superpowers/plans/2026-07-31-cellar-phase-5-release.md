# Cellar Phase 5 Release and Installation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Package Cellar as one embedded-web Windows service, deliver safe install/update/uninstall tooling, validate Cloudflare end-to-end, and install the verified release on the current PC.

**Architecture:** Build the React distribution before the Rust release and embed immutable assets in `cellar.exe`. Treat installation and update as durable state machines with recovery sets, keep cloudflared separate, and validate the same package in disposable Windows VMs before touching the current PC.

**Tech Stack:** Cargo release builds, rust-embed, PowerShell 7, Windows SCM, SQLite Online Backup API, SHA-256 manifests, Playwright, Hyper-V or an available disposable Windows VM runner.

---

### Task 1: Embed the production web application

**Files:**
- Create: `crates/cellar-api/src/assets.rs`
- Modify: `crates/cellar-api/src/router.rs`
- Modify: `crates/cellar-api/Cargo.toml`
- Modify: `web/vite.config.ts`
- Test: `crates/cellar-api/tests/assets.rs`

- [ ] **Step 1: Write failing asset tests**

```rust
#[tokio::test]
async fn serves_hashed_assets_and_spa_fallback() {
    let app = test_router();
    assert_eq!(get(&app, "/assets/app.hash.js").await.status(), 200);
    assert_eq!(get(&app, "/projects/example").await.status(), 200);
    assert_eq!(get(&app, "/api/v1/unknown").await.status(), 404);
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p cellar-api --test assets`

Expected: FAIL.

- [ ] **Step 3: Implement embedded assets**

```rust
#[derive(rust_embed::RustEmbed)]
#[folder = "../../web/dist/"]
pub struct WebAssets;

pub async fn serve_asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let selected = WebAssets::get(path).or_else(|| WebAssets::get("index.html"));
    asset_response(path, selected)
}
```

Give hashed assets long immutable caching and `index.html` no-cache. Never route `/api`, download, preview, events, or health misses to the SPA.

- [ ] **Step 4: Verify**

Run: `npm --prefix web run build; cargo test -p cellar-api --test assets`

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-api web/vite.config.ts
git commit -m "feat: embed production web assets"
```

### Task 2: Build reproducible release packages and manifests

**Files:**
- Create: `scripts/build-release.ps1`
- Create: `release/manifest.schema.json`
- Modify: `.gitignore`
- Test: `tests/windows-vm/verify-package.ps1`

- [ ] **Step 1: Write failing package verification**

```powershell
$package = Join-Path $PSScriptRoot '..\..\artifacts\cellar-win-x64'
& "$PSScriptRoot\verify-package.ps1" -Package $package
if ($LASTEXITCODE -eq 0) { throw 'Expected missing package verification to fail' }
```

- [ ] **Step 2: Verify failure**

Run: `pwsh -File tests/windows-vm/verify-package.ps1 -Package artifacts/cellar-win-x64`

Expected: non-zero because no package exists.

- [ ] **Step 3: Implement release build**

```powershell
$ErrorActionPreference = 'Stop'
npm --prefix web ci
npm --prefix web run build
cargo build --workspace --release --locked

$artifact = Join-Path $PSScriptRoot '..\artifacts\cellar-win-x64'
New-Item -ItemType Directory -Force -Path $artifact | Out-Null
Copy-Item target\release\cellar-service.exe "$artifact\cellar.exe"
$hash = (Get-FileHash "$artifact\cellar.exe" -Algorithm SHA256).Hash.ToLowerInvariant()
@{ version = $env:CELLAR_VERSION; files = @{ 'cellar.exe' = $hash } } |
  ConvertTo-Json -Depth 4 |
  Set-Content "$artifact\manifest.json" -Encoding utf8NoBOM
```

Make missing version, dirty generated assets, hash mismatch, or failed tests abort the package.

- [ ] **Step 4: Verify package**

Run: `pwsh -File scripts/build-release.ps1; pwsh -File tests/windows-vm/verify-package.ps1 -Package artifacts/cellar-win-x64`

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add scripts/build-release.ps1 release/manifest.schema.json tests/windows-vm/verify-package.ps1 .gitignore
git commit -m "build: package Cellar release"
```

### Task 3: Implement idempotent install and cloudflared configuration output

**Files:**
- Create: `scripts/install.ps1`
- Create: `crates/cellar-service/src/cli.rs`
- Create: `docs/operations/cloudflare-setup.md`
- Test: `tests/windows-vm/install.Tests.ps1`

- [ ] **Step 1: Write failing installer contract tests**

```powershell
Describe 'Cellar installer' {
  It 'preserves an existing valid install on rerun' {
    & $Install @ValidArgs
    $first = Get-FileHash "$env:ProgramFiles\Cellar\cellar.exe"
    & $Install @ValidArgs
    (Get-FileHash "$env:ProgramFiles\Cellar\cellar.exe").Hash | Should -Be $first.Hash
  }
}
```

- [ ] **Step 2: Verify failure**

Run: `Invoke-Pester tests/windows-vm/install.Tests.ps1`

Expected: FAIL.

- [ ] **Step 3: Implement install state machine**

```powershell
param(
  [Parameter(Mandatory)] [uri] $ExternalOrigin,
  [Parameter(Mandatory)] [uri] $TeamDomain,
  [Parameter(Mandatory)] [string[]] $AudienceTag,
  [Parameter(Mandatory)] [string] $BootstrapOwnerEmail,
  [Parameter(Mandatory)] [string] $StorageRoot,
  [int] $OriginPort = 9443,
  [int] $HealthPort = 9444
)

$ErrorActionPreference = 'Stop'
Assert-Administrator
Test-ReleaseManifest
Install-ProgramFiles
Register-CellarService
Invoke-CellarConfigure @PSBoundParameters
Set-CellarAcl
Invoke-ServiceIdentityPreflight
Write-CloudflaredRouteFragment
Start-Service Cellar
Assert-LoopbackHealth
```

Register `NT SERVICE\Cellar`, automatic delayed start, preshutdown, recovery actions, exclusive ports, Event Log source, Program Files read/execute ACL, ProgramData and storage ACL, origin key ACL, and Cloudflare fragment. Do not read or write tunnel credentials.

- [ ] **Step 4: Run installer tests in disposable VM**

Run: `Invoke-Pester tests/windows-vm/install.Tests.ps1`

Expected: PASS for clean install, rerun, denied roots, ACL, ports, certificate, service identity, readiness, and preserved data.

- [ ] **Step 5: Commit**

```powershell
git add scripts/install.ps1 crates/cellar-service/src/cli.rs docs/operations/cloudflare-setup.md tests/windows-vm/install.Tests.ps1
git commit -m "feat: add Windows installer"
```

### Task 4: Implement durable update and rollback

**Files:**
- Create: `scripts/update.ps1`
- Create: `crates/cellar-service/src/update_state.rs`
- Test: `tests/windows-vm/update.Tests.ps1`
- Test: `crates/cellar-e2e/tests/updater.rs`

- [ ] **Step 1: Write failing phase interruption matrix**

```powershell
$phases = 'quiesced','backup-created','service-stopped','binary-replaced','migrated','started'
foreach ($phase in $phases) {
  Invoke-UpdateWithCrash -After $phase
  Resume-Update
  Assert-ExactlyOneHealthyVersion
  Assert-DatabaseIntegrity
}
```

- [ ] **Step 2: Verify failure**

Run: `Invoke-Pester tests/windows-vm/update.Tests.ps1`

Expected: FAIL.

- [ ] **Step 3: Implement durable updater**

```powershell
$state = Read-UpdateState
switch ($state.Phase) {
  'start'          { Invoke-Quiesce; Save-Phase 'quiesced' }
  'quiesced'       { New-RecoverySet; Save-Phase 'backup-created' }
  'backup-created' { Stop-CellarGracefully; Save-Phase 'service-stopped' }
  'service-stopped'{ Install-NewBinary; Save-Phase 'binary-replaced' }
  'binary-replaced'{ Invoke-Migrations; Save-Phase 'migrated' }
  'migrated'       { Start-And-Validate; Save-Phase 'started' }
  'started'        { Confirm-Update; Remove-UpdateState }
}
```

On failure, restore executable, SQLite backup, and config as one set. Keep mutations disabled until migrations, recovery, preflight, and readiness pass. Retain three recovery sets. Reject non-expand-only migration manifests.

- [ ] **Step 4: Run update and fault tests**

Run: `Invoke-Pester tests/windows-vm/update.Tests.ps1; cargo test -p cellar-e2e --test updater`

Expected: PASS at every durable phase.

- [ ] **Step 5: Commit**

```powershell
git add scripts/update.ps1 crates/cellar-service/src/update_state.rs tests/windows-vm/update.Tests.ps1 crates/cellar-e2e/tests/updater.rs
git commit -m "feat: add recoverable Cellar updater"
```

### Task 5: Implement safe uninstall

**Files:**
- Create: `scripts/uninstall.ps1`
- Test: `tests/windows-vm/uninstall.Tests.ps1`

- [ ] **Step 1: Write failing preservation tests**

```powershell
It 'preserves ProgramData and project files by default' {
  $before = Get-TreeDigest $StorageRoot,$ProgramDataRoot
  & $Uninstall
  Get-Service Cellar -ErrorAction SilentlyContinue | Should -BeNullOrEmpty
  (Get-TreeDigest $StorageRoot,$ProgramDataRoot) | Should -Be $before
}
```

- [ ] **Step 2: Verify failure**

Run: `Invoke-Pester tests/windows-vm/uninstall.Tests.ps1`

Expected: FAIL.

- [ ] **Step 3: Implement explicit removal switches**

```powershell
param(
  [switch] $RemoveProgramData,
  [switch] $RemoveProjectFiles,
  [string] $ConfirmStorageRoot
)

Stop-And-RemoveCellarService
Remove-Item -LiteralPath "$env:ProgramFiles\Cellar" -Recurse
if ($RemoveProgramData) { Remove-ValidatedProgramData }
if ($RemoveProjectFiles) {
  Assert-ExactStorageRootConfirmation $ConfirmStorageRoot
  Remove-ValidatedStorageRoot
}
```

Resolve and verify every destructive target before removal. Never infer project-file deletion from ProgramData deletion.

- [ ] **Step 4: Run uninstall tests**

Run: `Invoke-Pester tests/windows-vm/uninstall.Tests.ps1`

Expected: PASS for default preservation, separate switches, wrong confirmation, reparse target rejection, and reinstall.

- [ ] **Step 5: Commit**

```powershell
git add scripts/uninstall.ps1 tests/windows-vm/uninstall.Tests.ps1
git commit -m "feat: add safe Cellar uninstall"
```

### Task 6: Automate Cloudflare and security smoke tests

**Files:**
- Create: `scripts/smoke-cloudflare.ps1`
- Create: `crates/cellar-e2e/tests/cloudflare_access.rs`
- Create: `docs/operations/security-checklist.md`

- [ ] **Step 1: Write failing smoke assertions**

```powershell
Assert-HttpStatus "$ExternalOrigin/" 302
Assert-OwnerBrowserAccess $ExternalOrigin
Assert-NonOwnerDenied $ExternalOrigin
Assert-LocalOriginImpersonationDenied
Assert-NoAnonymousHealthAtPublicOrigin
```

- [ ] **Step 2: Verify failure**

Run: `pwsh -File scripts/smoke-cloudflare.ps1 -ExternalOrigin $env:CELLAR_EXTERNAL_ORIGIN`

Expected: FAIL until the real route and checks exist.

- [ ] **Step 3: Implement smoke tests without logging credentials**

```powershell
param([Parameter(Mandatory)][uri]$ExternalOrigin)
$ErrorActionPreference = 'Stop'
Test-PublicRedirectToAccess $ExternalOrigin
Test-AuthenticatedOwnerSession $ExternalOrigin
Test-OriginCertificatePinning
Test-CloudflaredServiceAcl
Test-LogRedaction
```

Use a user-driven browser login or short-lived test identity supplied outside logs. Do not persist Access cookies or tunnel tokens.

- [ ] **Step 4: Run security smoke**

Run: `pwsh -File scripts/smoke-cloudflare.ps1 -ExternalOrigin $env:CELLAR_EXTERNAL_ORIGIN`

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add scripts/smoke-cloudflare.ps1 crates/cellar-e2e/tests/cloudflare_access.rs docs/operations/security-checklist.md
git commit -m "test: add Cloudflare security smoke"
```

### Task 7: Run acceptance, performance, and current-PC installation

**Files:**
- Create: `tests/windows-vm/run.ps1`
- Create: `crates/cellar-e2e/tests/scale.rs`
- Create: `crates/cellar-e2e/tests/large_upload.rs`
- Create: `docs/operations/install-report.md`

- [ ] **Step 1: Add measurable scale tests**

```rust
#[test]
fn one_hundred_thousand_entries_reconcile_within_reference_budget() {
    let elapsed = reconcile_fixture(100_000);
    assert!(elapsed < Duration::from_secs(300));
}

#[tokio::test]
async fn ten_gib_upload_resumes_and_matches_sha256() {
    let result = upload_generated_stream(10 * GIB, InterruptAt::Half).await;
    assert_eq!(result.source_sha256, result.server_sha256);
}
```

- [ ] **Step 2: Run full gates before installing**

Run:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
npm --prefix web ci
npm --prefix web run lint
npm --prefix web run typecheck
npm --prefix web run test
npm --prefix web run build
npm --prefix web run test:e2e
pwsh -File scripts/build-release.ps1
pwsh -File tests/windows-vm/run.ps1
```

Expected: every command exits `0`.

- [ ] **Step 3: Install the verified package on the current PC**

```powershell
pwsh -File scripts/install.ps1 `
  -ExternalOrigin $env:CELLAR_EXTERNAL_ORIGIN `
  -TeamDomain $env:CELLAR_TEAM_DOMAIN `
  -AudienceTag $env:CELLAR_AUD_TAG `
  -BootstrapOwnerEmail $env:CELLAR_OWNER_EMAIL `
  -StorageRoot $env:CELLAR_STORAGE_ROOT
```

Expected: service is running, health listener is live, readiness requests owner enrollment, and the cloudflared fragment is emitted.

- [ ] **Step 4: Complete owner enrollment and final smoke**

Run:

```powershell
pwsh -File scripts/smoke-cloudflare.ps1 -ExternalOrigin $env:CELLAR_EXTERNAL_ORIGIN
Get-Service Cellar | Format-List Status,StartType
```

Expected: owner can sign in remotely; non-owner is denied; service status is `Running`; start type is automatic; upload/download/Explorer/restart smoke passes.

- [ ] **Step 5: Record report and commit**

Write exact package hash, service version, OS build, storage volume identity, test command results, installation time, and known nonblocking limitations to `docs/operations/install-report.md`.

```powershell
git add tests/windows-vm/run.ps1 crates/cellar-e2e/tests/scale.rs crates/cellar-e2e/tests/large_upload.rs docs/operations/install-report.md
git commit -m "test: verify Cellar v1 release"
```

# Origin TLS Phase 5 acceptance

Run this acceptance only on a disposable Windows VM. It validates behavior that unit fault
injection cannot prove: real power loss, Windows token filtering, service-SID ACL access, and a
live cloudflared trust-anchor rollout.

## Prerequisites and evidence

- Take a VM snapshot and record the Windows build, filesystem, Rust version, Cellar commit, and
  cloudflared version in the test record.
- Install the `Cellar` Windows service so it runs as `NT SERVICE\Cellar` and grant no interactive
  user access to its TLS directory.
- Keep one ordinary medium-integrity PowerShell and one **Run as administrator** PowerShell open.
- Configure a test tunnel and Access application. Never use a production hostname or CA.
- Save command output, the four TLS file SHA-256 hashes, `.cellar-origin-tls.transaction` contents,
  and the directory listing after every reboot.

The installer must harden the ProgramData parent **before any Cellar TLS operation**. Do not rely on
the service to repair an initially broad inherited directory ACL:

```powershell
$path = 'C:\ProgramData\Cellar\tls'
$system = [System.Security.Principal.SecurityIdentifier]::new('S-1-5-18')
$admins = [System.Security.Principal.SecurityIdentifier]::new('S-1-5-32-544')
$cellar = [System.Security.Principal.SecurityIdentifier]::new(
  'S-1-5-80-3653650444-108827001-3922763823-736153100-155111920'
)
New-Item -ItemType Directory -Force $path | Out-Null
$acl = [System.Security.AccessControl.DirectorySecurity]::new()
$acl.SetAccessRuleProtection($true, $false)
$acl.SetOwner($admins)
$inheritance = [System.Security.AccessControl.InheritanceFlags]::ContainerInherit -bor
               [System.Security.AccessControl.InheritanceFlags]::ObjectInherit
$propagation = [System.Security.AccessControl.PropagationFlags]::None
$allow = [System.Security.AccessControl.AccessControlType]::Allow
$acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new(
  $system, [System.Security.AccessControl.FileSystemRights]::FullControl,
  $inheritance, $propagation, $allow
))
$acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new(
  $admins, [System.Security.AccessControl.FileSystemRights]::FullControl,
  $inheritance, $propagation, $allow
))
$acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new(
  $cellar, [System.Security.AccessControl.FileSystemRights]::Modify,
  $inheritance, $propagation, $allow
))
Set-Acl -LiteralPath $path -AclObject $acl

$actual = Get-Acl -LiteralPath $path
$expected = @{
  'S-1-5-18' = [int][System.Security.AccessControl.FileSystemRights]::FullControl
  'S-1-5-32-544' = [int][System.Security.AccessControl.FileSystemRights]::FullControl
  'S-1-5-80-3653650444-108827001-3922763823-736153100-155111920' =
    [int][System.Security.AccessControl.FileSystemRights]::Modify
}
$entries = @($actual.Access)
$invalid = @($entries | Where-Object {
  $sid = $_.IdentityReference.Translate(
    [System.Security.Principal.SecurityIdentifier]
  ).Value
  $_.IsInherited -or $_.AccessControlType -ne $allow -or
    -not $expected.ContainsKey($sid) -or [int]$_.FileSystemRights -ne $expected[$sid]
})
if (-not $actual.AreAccessRulesProtected -or $entries.Count -ne 3 -or $invalid.Count) {
  throw 'Unsafe Cellar TLS directory ACL; abort installation before any TLS operation.'
}
```

This constructs a fresh protected DACL, so inherited and pre-existing explicit ACEs are removed.
Expected: exactly the three allow ACEs above and no `Everyone`, `Authenticated Users`, or `Users`
ACE. Stop acceptance immediately if verification fails, because initial staging files inherit from
this parent before their handle-applied protected DACL is installed.

Build once from the repository root:

```powershell
cargo build -p cellar-service --release
cargo test -p cellar-service --test origin_tls -- --list
```

## Identity and ACL checks

In ordinary PowerShell, confirm the Administrators SID is deny-only or absent, then prove public
rotation is rejected without creating a CA:

```powershell
whoami /groups
cargo test -p cellar-service --test origin_tls medium_integrity_process_cannot_rotate_the_origin_ca -- --exact --nocapture
cargo test -p cellar-service --lib tls::tests::deleting_all_established_files_fails_closed_until_explicit_rotation -- --exact --nocapture
```

In elevated PowerShell, confirm the Administrators SID is enabled and run the production ACL,
renewal, rotation, and protected cross-process tests:

```powershell
whoami /groups
cargo test -p cellar-windows --test key_acl windows::handle_applied_dacl_has_no_broad_or_inherited_access -- --exact --nocapture
cargo test -p cellar-service --test origin_tls -- --ignored --nocapture
cargo test -p cellar-service --lib tls::tests::protected_cross_process_lock_serializes_production_initialization -- --ignored --exact --nocapture
```

Expected: ordinary rotation returns `AdministratorRequired`; elevated tests pass; the CA and leaf
key DACLs are protected and contain only SYSTEM, Administrators, and `NT SERVICE\Cellar`; concurrent
processes observe one CA serial and leave no transaction marker. Initial establishment also creates
a protected `.cellar-origin-tls.established` marker. Record its content and SHA-256 hash. Deleting all
four PEM files while that marker remains must produce `EstablishedMaterialMissing` without creating
any new PEM file; only the elevated explicit rotation path may recover, and it must preserve the
existing establishment-marker hash as audit evidence.
If first-time rotation commits its bundle but marker publication fails, the API must return the
explicit `RotationCommitted` error with committed material and the cloudflared-update-required
signal. Treat that as a changed trust anchor, update cloudflared, and verify the next startup
backfills the marker without generating another CA.

## Real power-loss matrix

Use a Phase 5 fault-build of the service that pauses at the named journal boundary. The pause hook
must only block; it must not skip flushes, renames, or ACL operations. Exercise bundle renewal for
the four-file rows and explicit CA rotation for the five-file rows. Wait until the stated on-disk
evidence appears, then use the hypervisor's **Power Off** action (not guest shutdown). Restore the
baseline snapshot before each row.

| Boundary | Evidence immediately before power-off | Expected result after reboot and service start |
| --- | --- | --- |
| Renewal prepared marker | marker is `prepared:<mask>:4`; all four PEM `.cellar-stage` files exist | original four files restored byte-for-byte; stages, backups, and marker removed |
| Renewal CA cert target-to-backup | `prepared:<mask>:4`; CA cert backup exists; CA cert target absent | original bundle restored; same CA hash |
| Renewal CA key target-to-backup | `prepared:<mask>:4`; CA cert replaced; CA key backup exists | original bundle restored; same CA hash; key DACL remains protected |
| Renewal leaf cert target-to-backup | `prepared:<mask>:4`; first two targets replaced; leaf cert backup exists | original bundle restored; same CA and leaf hashes |
| Renewal leaf key target-to-backup | `prepared:<mask>:4`; first three targets replaced; leaf key backup exists | original bundle restored; same CA and leaf hashes; key DACL remains protected |
| Each renewal staged-to-target rename | `prepared:<mask>:4`; corresponding backup and replacement target exist | original bundle restored at every one of the four rename boundaries |
| Renewal committed marker replacement | marker is `committed:<mask>:4`; all four new PEM targets exist | new complete bundle retained; backups, stages, and marker removed |
| Each renewal cleanup step | `committed:<mask>:4` remains while at least one backup/stage exists | new complete bundle retained; cleanup finishes on this or the next start |
| CA rotation prepared marker and pending stage | marker is `prepared:<mask>:5`; four PEM stages and the protected pending-record `.cellar-stage` sibling exist | original four PEM files retained byte-for-byte; pending record, all stages and backups, and marker removed |
| CA rotation pending-record staged-to-target rename | marker is `prepared:<mask>:5`; all four replacement PEM targets and the pending-record target exist | all five files roll back atomically; original CA remains active and no pending rotation is reported |
| CA rotation committed marker replacement | marker is `committed:<mask>:5`; all four new PEM targets and the protected pending-record target exist | new complete bundle and its matching pending record are retained; backups, stages, and marker removed |
| Each CA rotation cleanup step | `committed:<mask>:5` remains while at least one backup/stage exists | new complete bundle and pending record are retained; cleanup finishes on this or the next start |
| Establishment marker publish | complete four-file bundle exists; protected establishment marker stage is durable | same bundle retained; establishment marker publication finishes without generating another CA |

After every reboot run:

```powershell
Get-ChildItem 'C:\ProgramData\Cellar\tls' -Force
Get-FileHash 'C:\ProgramData\Cellar\tls\*.pem' -Algorithm SHA256
sc.exe query Cellar
```

No outcome may contain a mixed CA/leaf generation, replace the CA during ordinary startup, delete
recovery evidence after a metadata error, or leave a readable private key for an ordinary user.

## cloudflared continuity and explicit rotation

1. Point the test tunnel ingress `originRequest.caPool` at the current Cellar CA PEM and route the
   hostname to the local HTTPS origin. Start cloudflared and make an authenticated request through
   Access. Record success and the CA SHA-256 hash.
2. Exercise normal reuse and leaf renewal. Repeat the request and confirm the CA hash and caPool file
   are unchanged.
3. Exercise the near-CA-expiry fixture. Confirm `CaRotationRequired`, a still-valid served leaf, a
   successful request, and byte-for-byte unchanged TLS files.
4. From elevated PowerShell invoke the administrator rotation path. Record the new CA hash and the
   returned `cloudflared_ca_pool_and_route_update_required` signal. Confirm the protected durable
   pending-rotation record contains the same CA SHA-256 fingerprint and caPool certificate.
5. Before changing cloudflared, confirm the old caPool rejects the rotated origin. Atomically update
   caPool and the managed route fragment, validate the cloudflared configuration, then restart it.
6. Confirm authenticated traffic succeeds, the served leaf verifies only under the new CA, and a
   service restart does not rotate either certificate again. Call
   `acknowledge_origin_ca_rotation` with the recorded fingerprint only after deployment; a wrong
   fingerprint must preserve the record, while the matching acknowledgment durably removes it.

Attach the hashes, Access request results, cloudflared validation output, service logs, and completed
power-loss matrix to the Phase 5 release evidence.

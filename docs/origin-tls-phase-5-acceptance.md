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
must only block; it must not skip flushes, renames, or ACL operations. Start one bundle renewal, wait
until the stated on-disk evidence appears, and use the hypervisor's **Power Off** action (not guest
shutdown). Restore the baseline snapshot before each row.

| Boundary | Evidence immediately before power-off | Expected result after reboot and service start |
| --- | --- | --- |
| Prepared marker | marker is `prepared:<mask>`; all four `.cellar-stage` files exist | original four files restored byte-for-byte; stages, backups, and marker removed |
| CA cert target-to-backup | prepared marker; CA cert backup exists; CA cert target absent | original bundle restored; same CA hash |
| CA key target-to-backup | prepared marker; CA cert replaced; CA key backup exists | original bundle restored; same CA hash; key DACL remains protected |
| Leaf cert target-to-backup | prepared marker; first two targets replaced; leaf cert backup exists | original bundle restored; same CA and leaf hashes |
| Leaf key target-to-backup | prepared marker; first three targets replaced; leaf key backup exists | original bundle restored; same CA and leaf hashes; key DACL remains protected |
| Each staged-to-target rename | prepared marker; corresponding backup and replacement target exist | original bundle restored at every one of the four rename boundaries |
| Committed marker replacement | marker is `committed:<mask>`; all four new targets exist | new complete bundle retained; backups, stages, and marker removed |
| Each cleanup step | committed marker remains while at least one backup/stage exists | new complete bundle retained; cleanup finishes on this or the next start |
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
   returned `cloudflared_ca_pool_and_route_update_required` signal.
5. Before changing cloudflared, confirm the old caPool rejects the rotated origin. Atomically update
   caPool and the managed route fragment, validate the cloudflared configuration, then restart it.
6. Confirm authenticated traffic succeeds, the served leaf verifies only under the new CA, and a
   service restart does not rotate either certificate again.

Attach the hashes, Access request results, cloudflared validation output, service logs, and completed
power-loss matrix to the Phase 5 release evidence.

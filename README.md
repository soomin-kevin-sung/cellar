# Cellar

Cellar is a Windows-first, single-owner file hub. A Rust server stores files and
SQLite metadata on one local NTFS volume, serves an embedded React application,
and accepts remote traffic through a separately managed Cloudflare Tunnel and
Cloudflare Access application.

Cellar always binds to a loopback address. Do not expose its port through a
router, firewall rule, reverse proxy on a LAN address, or a public interface.

## Prerequisites

- Windows with PowerShell 5.1 or later.
- An existing directory on a fixed local NTFS volume for Cellar data. The data
  root itself must not be a symlink or reparse point.
- Rust 1.93 with `rustfmt` and `clippy`. The checked-in
  `rust-toolchain.toml` selects these automatically when using `rustup`.
- Node.js `20.19+`, `22.13+`, or `24+`, with npm.
- For remote use, a Cloudflare account, an active domain on Cloudflare, a Zero
  Trust organization and identity provider, and a separately installed current
  `cloudflared` release.

Cloudflare setup is intentionally outside Cellar. Follow the
[named Tunnel and Access runbook](docs/cloudflare-access-setup.md); Cellar does
not install, configure, or supervise `cloudflared`.

## Configuration

Create the data root before starting Cellar, copy `config.example.toml` to a
private operator-controlled location, and edit all placeholders. This is the
complete accepted schema:

```toml
bind = "127.0.0.1:8787"
external_origin = "https://files.example.com"
data_root = "D:/CellarData"
database_path = "D:/CellarData/.cellar/cellar.db"

[access]
team_domain = "https://example.cloudflareaccess.com"
audience = "replace-with-access-application-aud"
owner_email = "owner@example.com"
```

Configuration rules:

- `bind` must be an IP loopback socket such as `127.0.0.1:8787` or
  `[::1]:8787`. Hostnames, LAN addresses, `0.0.0.0`, and public addresses are
  rejected.
- `external_origin` is the exact public HTTPS origin used by the browser. Use a
  lower-case hostname with no path, query, fragment, explicit default port, or
  trailing slash.
- `data_root` and `database_path` must be absolute. The database must be a file
  beneath `<data_root>/.cellar`; keep it on the same volume as project files.
- `team_domain` is exactly
  `https://<team-name>.cloudflareaccess.com`, without a trailing slash.
- `audience` is the Access application's Application Audience (AUD) tag, with
  no added whitespace.
- `owner_email` must be the one identity allowed by the Access policy. Cellar
  trims it and lowercases ASCII characters before comparing it with the
  verified Access token email; storing the normalized lower-case form keeps the
  mapping unambiguous.

The tunnel token and tunnel credentials do not belong in this file or in the
Cellar database.

## Build

The frontend must be built before the Rust release because `web/dist` is
embedded into the executable at Rust compile time:

```powershell
npm --prefix web ci
npm --prefix web run build
cargo build --release
```

Repeat both build steps after any frontend change. The release executable is
`target\release\cellar.exe`.

## Start locally

For development, the helper rebuilds the frontend, temporarily sets
`CELLAR_CONFIG`, and runs the Rust application:

```powershell
.\scripts\run-dev.ps1 -ConfigPath .\config.toml
```

For a release start:

```powershell
$env:CELLAR_CONFIG = (Resolve-Path -LiteralPath .\config.toml).Path
$env:RUST_LOG = 'cellar=info'
.\target\release\cellar.exe
```

Keep that process running independently from `cloudflared`. A local request to
the UI shell can reach the loopback listener, but every `/api` request still
requires a valid Cloudflare Access assertion. Use the configured public HTTPS
hostname for the real user flow.

Stop Cellar with Ctrl+C (or the normal stop action of its process manager) and
wait for it to exit. Cellar stops accepting requests gracefully and closes
SQLite before exit; forced process termination is not a consistent shutdown.

## Data layout

With the sample paths, Cellar manages:

```text
D:\CellarData\
|- projects\
|  `- <project-uuid>\
|     `- files\
|        `- <validated-file-name>
`- .cellar\
   |- cellar.db
   |- cellar.db-wal             # may exist while Cellar is running
   |- cellar.db-shm             # may exist while Cellar is running
   `- uploads\
      `- <upload-uuid>.part
```

Project names and upload state live in SQLite. Published file metadata is read
from the real files under `projects`; `.part` files are resumable upload
staging. Do not rename, move, edit, or selectively synchronize managed entries
while Cellar is running.

## Backup and restore boundaries

Treat the complete data root as one consistency unit: project files, `.cellar`
database files, and upload staging must be captured together. The safe sequence
is:

1. Stop `cloudflared` so no new remote request arrives.
2. Stop Cellar gracefully and wait for the process to exit.
3. Copy or snapshot the entire configured `data_root` as one unit.
4. Back up the Cellar config and the separately managed Cloudflare tunnel
   credentials/configuration according to their own access controls.

Do not create a backup by copying only `cellar.db`, only `projects`, or only
completed files. Do not copy a live SQLite database and its WAL independently.
For restore, keep Cellar and `cloudflared` stopped, restore the whole data-root
snapshot to the configured absolute path, restore the matching config, then
start Cellar before the tunnel.

## Logs

Cellar writes structured JSON logs to the process output. `RUST_LOG` controls
filtering and defaults to `cellar=info`; for example, use `cellar=debug` only
during focused troubleshooting. Startup failures are emitted as a small JSON
record on standard error with a closed error code. Cellar does not create or
rotate log files, so configure capture, retention, and access permissions in
the console host or Windows process manager. Avoid enabling verbose
`cloudflared` request logging unless necessary because it can include sensitive
request details.

## Upgrade order

1. Stop `cloudflared` to drain the public entry point.
2. Stop Cellar gracefully and wait for exit.
3. Back up the complete data root and the separate config/connector material.
4. Update the source or release files, then run `npm --prefix web ci`.
5. Run `npm --prefix web run build`, then `cargo build --release` in that order.
6. Start the new Cellar binary. It applies bundled database migrations before
   listening; inspect its logs and confirm a direct unauthenticated API request
   is rejected with `401`.
7. Start the separately managed `cloudflared` connector and repeat the public
   Access verification in the runbook.

Do not start an older Cellar binary against data that a newer version has
migrated. Restore the pre-upgrade snapshot before rolling back.

## Checks

After installing dependencies, run the repeatable repository checks from the
repository root:

```powershell
.\scripts\check.ps1
```

The script performs formatting, Rust tests and linting, frontend tests and
build checks, and the release build. It stops on the first failing command and
does not install dependencies or provision services.

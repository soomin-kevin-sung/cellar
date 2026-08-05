# Cellar

## 무료 임시 주소로 실행 (Windows)

릴리스 빌드 후 저장소 루트에서 다음 한 줄로 실행합니다.

```powershell
.\dev.ps1
```

처음 실행할 때 `cloudflared`가 없으면 공식 휴대용 실행 파일을 사용자 폴더에 자동 설치하고 SHA-256을 검증합니다. 관리자 권한은 필요하지 않습니다. 실행이 끝나면 콘솔에 매번 새로 발급된 `trycloudflare.com` 주소와 최초 관리자 암호가 표시됩니다. 사용자 이름은 `cellar`입니다. 최초 로그인 후 계정은 SQLite에 유지되며, 이후에는 웹 관리자 화면에서 다른 사용자 계정을 관리합니다. `Ctrl+C`로 Cellar와 터널을 함께 종료합니다.

Quick Tunnel은 도메인과 Cloudflare 계정 없이 무료로 쓸 수 있지만, 주소가 실행할 때마다 바뀌며 Cloudflare가 개발 및 테스트 용도로만 제공합니다. 중요한 파일은 별도로 백업하고 장기 공개 서비스 용도로 사용하지 마세요.

### Vercel 고정 진입 주소

Vercel 계정에 한 번 로그인한 뒤 프로젝트 이름을 지정하면, Cellar가 시작될 때마다 고정 Vercel 주소의 임시 리다이렉트를 새 Quick Tunnel 주소로 자동 갱신합니다. Vercel은 파일을 중계하지 않습니다.

```powershell
npx vercel login
.\dev.ps1 -VercelProject cellar-entry
```

프로젝트 이름을 매번 입력하지 않으려면 사용자 환경 변수로 저장할 수 있습니다.

```powershell
[Environment]::SetEnvironmentVariable('CELLAR_VERCEL_PROJECT', 'cellar-entry', 'User')
```

Vercel 배포가 실패해도 Quick Tunnel과 Cellar는 계속 실행되며 콘솔에 임시 주소가 표시됩니다.

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

## MVP behavior

- Projects group files stored on this Windows PC.
- Uploads are sent sequentially in 32 MiB requests so files larger than a
  Cloudflare request limit can still be transferred. If any request fails,
  retry starts from the beginning.
- Files are first written to an application-owned `.part` file and published
  only after the request completes successfully.
- Failed uploads are cleaned immediately; leftovers from a forced process exit
  are removed on the next startup.
- Existing filenames are never overwritten.
- Upload resume, cancellation controls, filesystem watching, rename, move,
  delete, tags, search, and previews are intentionally deferred.

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

The script surface is intentionally small:

```text
dev.ps1                     # run remote development with Vite hot reload
deploy.ps1                  # build and copy the production runtime
scripts/start-cellar.ps1    # internal server and tunnel launcher
scripts/check.ps1           # optional full project checks
```

Most work uses only `dev.ps1` and `deploy.ps1`.

The frontend must be built before the Rust release because `web/dist` is
embedded into the executable at Rust compile time:

```powershell
npm --prefix web ci
npm --prefix web run build
cargo build --release
```

Repeat both build steps after any frontend change. The release executable is
`target\release\cellar.exe`.

## Deploy on Windows

Build and deploy a standalone runtime under `D:\Cellar`:

```powershell
.\deploy.ps1 -Destination 'D:\Cellar' -VercelProject 'cellar-entry'
```

The installed layout keeps the executable, tunnel helper, configuration, data,
and logs beneath `D:\Cellar`. The deployment records the port and Vercel project
in `config\launcher.json`. Start it with:

```powershell
& 'D:\Cellar\cellar.ps1' -Password '<at-least-16-characters>'
```

The start script downloads `cloudflared` when needed, creates a Quick Tunnel,
updates the Vercel fixed entry URL, writes the runtime configuration, and starts
Cellar. `CELLAR_PASSWORD` can be used instead of the `-Password` argument.

Running `deploy.ps1` again updates the executable and start script without
deleting the existing `data` directory.

## Start locally

Run the isolated remote development environment on port `8788` with its own
data under `D:\Cellar-dev\data`:

```powershell
.\dev.ps1
```

The script starts Vite with hot reload, builds the Rust backend, creates a
separate Quick Tunnel, and updates `https://cellar-entry.vercel.app/dev`.
Frontend changes appear remotely without restarting the script. Restart it only
after Rust backend changes. Production remains on port `8787` with data under
`D:\Cellar\data`. Both launchers share only
`D:\Cellar\config\vercel-routes.json`, which preserves both redirect targets
when either temporary tunnel URL changes. Use `CELLAR_DEV_PASSWORD` or the
`-Password` parameter when a stable development bootstrap password is wanted.

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

Project names live in SQLite. Published file metadata is read from the real
files under `projects`; `.part` files are temporary upload staging and are
removed at startup. Do not rename, move, edit, or selectively synchronize
managed entries while Cellar is running.

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

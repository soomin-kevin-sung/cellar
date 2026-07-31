# Cellar Design Specification

**Date:** 2026-07-31

**Status:** Approved for implementation planning

## 1. Product Definition

Cellar is a single-owner, self-hosted project file hub. It runs as a Windows service on the owner's PC and provides secure remote access to files through a responsive web interface.

Cellar owns one user-selected storage root. Within that root, it creates project directories, maintains project metadata, supports resumable uploads and streaming downloads, provides safe previews, and reconciles changes made directly through Windows Explorer.

### Goals

- Organize files into catalog-style projects.
- Access project files remotely from desktop and mobile browsers.
- Upload files without an application-level file size limit.
- Resume interrupted uploads and downloads.
- Preview common safe file formats.
- Allow the storage root to remain usable through Windows Explorer.
- Install and run automatically as a Windows service.
- Preserve clear boundaries for future Linux and macOS support.

### Non-goals for the MVP

- Public share links
- Multiple users, roles, or collaboration
- Automatic multi-device folder synchronization
- Browser-based file editing
- Office document conversion or editing
- Archive extraction
- Remote desktop, terminal, process management, or general PC administration
- Network or NAS storage roots
- Microservices

## 2. Confirmed Product Decisions

- **Primary platform:** Windows
- **Future platforms:** Linux and macOS through platform adapters
- **Owner model:** One configured owner
- **Remote ingress:** Cloudflare Tunnel
- **External authentication:** Cloudflare Access
- **Backend:** Rust, Axum, and Tokio
- **Frontend:** React and TypeScript
- **Metadata database:** SQLite
- **Deployment:** One Cellar Windows service
- **Storage:** One Cellar-managed local storage root
- **Project model:** Catalog projects with name, description, cover, status, timestamps, file count, and total size
- **Project states:** Active and archived; deletion is represented separately
- **File management:** Upload, download, folder creation, rename, move, copy, trash, restore, and safe preview
- **Explorer integration:** Direct file changes inside each project's `files` directory are supported and reconciled

## 3. System Architecture

```text
Remote browser
  -> Cloudflare Access
  -> Cloudflare Tunnel
  -> cloudflared Windows service with Access validation enabled
  -> 127.0.0.1 Cellar Windows service
       |- Authentication and CSRF boundary
       |- Project catalog
       |- Upload and download engine
       |- Safe preview service
       |- Filesystem watcher and reconciler
       |- Platform filesystem adapter
       |- SQLite repository and operation journal
       `- Server-sent event stream
```

Cellar is a modular monolith. The modules have explicit interfaces, but they run in one process and are deployed as one executable.

Microservices are intentionally excluded because all components share one local filesystem, one SQLite database, one owner, and one host. Splitting them into network services would add service discovery, internal authentication, port management, distributed recovery, multi-process database contention, and deployment complexity without providing useful independent scaling.

Potential future extraction points are CPU-intensive preview workers, multi-host indexing, and a central control plane for multiple PCs.

## 4. Security Model

### 4.1 Threat boundary

Cellar protects the service from unauthenticated remote users, malicious uploaded content, path traversal, cross-site requests, and unprivileged local users.

The Windows local administrator and administrator-controlled local processes are trusted. A compromised local administrator is outside the MVP threat model.

### 4.2 Cloudflare and origin authentication

The service listens only on `127.0.0.1`. No router port or public origin IP is opened.

Cloudflare Access permits only the configured owner's identity. The `cloudflared` route enables origin-side Access validation with:

- `access.required: true`
- The expected Cloudflare team name
- The Access application's audience tag

Cellar independently validates `Cf-Access-Jwt-Assertion`:

- Signature and allowed algorithm
- Issuer
- Audience
- Expiration and not-before time
- Token type
- Configured owner email or subject

Cellar caches Cloudflare JWKS keys with rotation support. Missing keys, stale keys that cannot be refreshed, malformed claims, or validation errors fail closed.

All state-changing APIs require a valid Access identity and strict same-origin protection using an anti-CSRF token and `Origin` validation. CORS is denied by default.

### 4.3 Filesystem security

The API accepts a project ID, parent entry ID, and validated relative names. It never accepts an arbitrary absolute filesystem path.

The Windows adapter rejects:

- Parent traversal
- Drive-qualified paths
- UNC paths
- NUL characters
- Alternate data stream separators
- Windows reserved device names
- Names ending in a dot or space
- Symlinks, junctions, mount points, and other reparse points

Project directories use UUIDs rather than display names. Security-sensitive operations validate opened file handles against the configured storage volume and project root to reduce check-then-use races.

Cellar runs as a dedicated low-privilege Windows service identity, preferably `NT SERVICE\Cellar`, with access only to its program data and configured storage root.

The installer grants the configured interactive Windows owner and the Cellar service identity access to the project storage. It does not grant broad access to other local users.

### 4.4 Safe preview boundary

The MVP previews only an allowlist:

- PNG, JPEG, and WebP images
- Supported audio and video formats through browser-native playback
- PDF
- Escaped plain text and source code

HTML, SVG, XML, unknown types, and executable formats are served only as attachments. Preview responses use `X-Content-Type-Options: nosniff`, restrictive content security policy, and sandboxing where applicable.

The MVP does not unpack archives or execute external document converters.

## 5. Storage Layout

```text
<storage-root>/
|- projects/
|  `- <project-uuid>/
|     `- files/
|        `- user-visible folders and files
`- .cellar/
   |- staging/
   `- trash/
      `- <project-uuid>/
```

```text
C:\Program Files\Cellar\
`- cellar.exe

C:\ProgramData\Cellar\
|- cellar.db
|- config.toml
|- logs/
`- backups/
```

Original file content is stored in the filesystem, not in SQLite. SQLite is always placed on a local disk and never on SMB or NAS storage.

Users may directly manage files within `projects/<project-uuid>/files` through Windows Explorer. The `.cellar` directory is private implementation storage and must not be edited manually.

## 6. Domain and Persistence Model

### 6.1 Project

```text
project
  id
  name
  description
  status
  version
  created_at
  updated_at
  deleted_at
```

Project status is `active` or `archived`. Project deletion uses a separate deletion timestamp and trash operation.

### 6.2 File entry

```text
file_entry
  id
  project_id
  parent_id
  exact_name
  kind
  volume_serial
  filesystem_file_id
  size
  mtime_ns
  hash
  hash_state
  state
  revision
  scan_generation
  observed_at
```

File states are:

```text
live | settling | missing | trashed | unsupported
```

The logical Cellar ID and physical path are separate.

- `parent_id + exact_name` is the canonical logical location.
- Relative path is computed or cached for reads; it is not a second source of truth.
- On Windows, `(volume_serial, FILE_ID_128)` maintains logical identity across Explorer rename and move operations.
- An atomic editor replacement at the same path with a new filesystem ID becomes a new revision of the same logical file.
- Ambiguous hard links or identity matches fall back to delete-plus-create behavior.

Cellar preserves the exact on-disk filename. It does not normalize or rewrite names. Windows-compatible ordinal case-insensitive comparison is implemented with a custom SQLite collation or stored comparison key. SQLite's built-in `NOCASE` is not used as a substitute for Windows filename comparison.

Unsupported names are recorded as `unsupported` rather than deleted.

### 6.3 Project cover

```text
project_cover
  project_id
  file_entry_id
```

A composite foreign key guarantees that a cover belongs to the same project. Removing the referenced file automatically clears the cover.

### 6.4 Upload session

```text
upload_session
  id
  project_id
  destination_parent_id
  destination_name
  expected_size
  committed_offset
  expected_hash
  state
  expires_at
```

The upload destination is uniquely reserved while the session is active.

`expected_hash` is optional because calculating a whole-file digest in the browser can be expensive for very large files. The server always calculates and stores the final SHA-256 digest; when the client supplies an expected digest, the server also compares it before commit.

### 6.5 Operation journal

```text
operation
  id
  project_id
  kind
  state
  payload
  error
  created_at
  updated_at
```

Operation states are:

```text
pending -> fs_applied -> complete
                    `-> failed
```

All web-initiated mutations create an operation record before changing the filesystem. Startup recovery resumes or reconciles incomplete operations.

### 6.6 Trash

```text
trash_item
  id
  project_id
  root_entry_id
  original_path_snapshot
  storage_path
  deleted_at
  purge_after
  state
```

Web deletion moves the target to the same-volume Cellar trash. Restore never overwrites an existing destination and returns a conflict instead.

The default trash retention period is 30 days and is configurable. Purging is a separate audited operation and never occurs before `purge_after`.

Explorer deletion is detected and audited but is not recoverable by Cellar.

Project deletion cancels active uploads, atomically moves the project directory to project trash, and marks the database record deleted only after the move succeeds.

### 6.7 Audit event

```text
audit_event
  sequence
  event_id
  operation_id
  source
  project_id
  target_id
  path_snapshot
  action
  result
  occurred_at
  details
```

Sources are `web`, `explorer`, `reconcile`, and `system`.

Audit records survive project deletion. Web operations are fully audited. Explorer audit is best effort and is not presented as a forensic record of every short-lived local change.

### 6.8 SQLite operation

SQLite uses:

- WAL mode
- Foreign keys enabled on every connection
- Busy timeout
- A small read pool and one controlled writer
- Versioned migrations
- SQLite Online Backup API for live backups

Restoring a backup always triggers a full filesystem reconciliation.

## 7. Explorer Synchronization and Reconciliation

The Windows watcher is a latency optimization, not the authoritative change log.

### Normal event flow

```text
Windows change event
  -> debounce
  -> identify entry using filesystem identity
  -> settling
  -> wait for stable size and modification time
  -> update SQLite catalog
  -> publish UI event
```

Files being copied by Explorer remain `settling` until size and modification time stop changing. Cellar does not preview or hash a settling file.

### Recovery scan

- A full scan runs at service startup.
- Periodic project scans correct missed events. The default interval is 15 minutes and is configurable.
- Watcher overflow or error immediately marks the affected project dirty and starts reconciliation.
- Each scan uses a new `scan_generation`.
- Entries are incrementally upserted during the scan.
- Entries not observed are marked missing only after the scan completes successfully.
- Changes observed during a scan cause the affected subtree to be scanned again before the project becomes clean.

The filesystem remains the final authority. API requests revalidate the actual target instead of trusting cached metadata. ETags are concurrency hints, not locks.

## 8. Large File Transfer

"No application-level file size limit" means Cellar does not impose an arbitrary whole-file cap. Real limits are available disk space, filesystem limits, browser limits, and transport limits.

Cloudflare's current Free account request body limit is 100 MB, so Cellar never uploads an entire large file in one request. The protocol uses 32 MiB chunks.

### Upload flow

```text
create session
  -> reserve destination
  -> upload sequential 32 MiB chunks
  -> validate offset, length, and SHA-256 digest
  -> flush staging file
  -> validate final size and SHA-256
  -> record committing operation
  -> atomically rename within the same volume
  -> update catalog
  -> mark complete
```

All sizes and offsets use 64-bit integers.

The server accepts the next committed offset. A duplicate retry whose bytes and digest already match succeeds idempotently and returns the current committed offset. Overlapping, skipped, or mismatched chunks are rejected.

One file uploads sequentially in the MVP. Up to three different files upload concurrently by default, and the setting is configurable. The server reserves disk capacity conservatively and rejects new work when free space would fall below a configurable safety reserve, which defaults to 5 GiB.

Incomplete sessions expire after seven days by default. Expiration is configurable, and cleanup is audited.

### Download flow

Downloads and media preview support:

- Streaming without loading the whole file into memory
- `Range`
- `ETag`
- `If-Range`
- Resume after interruption
- Correct `Content-Disposition`

Cloudflare currently enforces no response body size limit, although cache limits remain separate. Cellar does not depend on CDN caching for private files.

## 9. API and Error Model

The HTTP API is versioned under `/api/v1`.

Errors use a stable envelope:

```json
{
  "code": "destination_conflict",
  "message": "A file with the same name already exists.",
  "requestId": "uuid",
  "details": {}
}
```

Important status mappings:

- `401` or `403`: authentication or authorization failure
- `409`: destination, ETag, restore, or operation conflict
- `416`: invalid byte range
- `507`: insufficient storage
- `503`: storage root or database temporarily unavailable

The service enters a degraded state rather than exiting when the storage root is temporarily unavailable. Readiness and the UI expose the reason; mutating operations remain disabled until recovery.

Every request receives a request ID linked to structured logs and audit events.

Server-sent events notify the frontend about uploads, reconciliation, catalog changes, degraded state, and recovery.

## 10. Frontend Experience

The interface should feel like a polished personal project archive rather than a server administration console.

### Main navigation

- Projects
- Recent items
- Transfers
- Trash
- Settings

### Project list

- Cover-driven project cards
- Searchable list view
- Active and archived filters
- Project file count and total size
- Storage usage summary

### Project detail

- Cover, name, description, status, size, and file count
- Breadcrumb navigation
- Table and grid file views
- Drag-and-drop upload
- New folder, rename, move, copy, download, trash, and restore
- Right-side file information and preview panel

### Transfer experience

- Persistent transfer panel across navigation
- Clear per-file progress and current phase
- Interrupted-session resume
- Actionable error messages

### Responsive behavior

Desktop uses a compact sidebar, project content area, and preview panel. Mobile collapses navigation and exposes primary file actions in a reachable bottom action surface.

Destructive actions require explicit confirmation. Connection loss places the UI in a visible read-only state until synchronization completes.

The visual direction uses restrained wine and slate colors, deliberate typography and spacing, subtle motion, and content-led hierarchy. Accessibility, keyboard navigation, visible focus states, and reduced-motion preferences are required.

## 11. Codebase Structure

```text
cellar/
|- crates/
|  |- cellar-core/
|  |- cellar-api/
|  |- cellar-auth/
|  |- cellar-storage/
|  |- cellar-windows/
|  `- cellar-db/
|- web/
|- migrations/
|- scripts/
|  |- install.ps1
|  |- update.ps1
|  `- uninstall.ps1
|- tests/
|- Cargo.toml
`- README.md
```

### Rust

- Axum for HTTP routing and middleware
- Tokio for asynchronous I/O and bounded background tasks
- SQLx for SQLite access and migrations
- Serde for configuration and API types
- Tracing for structured logs
- Windows APIs behind `cellar-windows`

### Web

- React and TypeScript
- Vite
- TanStack Router and Query
- Accessible headless primitives
- CSS variables and a utility-based styling layer

The production web build is embedded in `cellar.exe`. Node.js is needed for development and build only, not on the installed machine.

## 12. Windows Installation and Lifecycle

### Install

The administrator-run `install.ps1`:

1. Verifies or builds a release package.
2. Installs `cellar.exe` under `C:\Program Files\Cellar`.
3. Creates `C:\ProgramData\Cellar` directories.
4. Registers an automatically starting Windows service.
5. Applies least-privilege ACLs to program data and the selected storage root.
6. Starts the service.
7. Verifies local health and readiness endpoints.

### Update

The updater:

1. Gracefully stops the service.
2. Preserves the previous executable.
3. Atomically installs the new executable.
4. Applies versioned database migrations.
5. Starts the service and runs health checks.
6. Restores the previous executable when startup or migration validation fails where rollback is safe.

Database migration design must distinguish reversible executable rollback from irreversible schema changes. Destructive migrations require an explicit backup and forward-recovery plan.

### Uninstall

Uninstall removes the Windows service and program files. It preserves `ProgramData`, SQLite data, backups, and project files unless an explicit data-removal option is supplied.

### Cloudflare

`cloudflared` runs as a separate Windows service. Its tunnel token is not stored in Cellar configuration. Cellar may detect and report configuration state but does not own Cloudflare credentials.

## 13. Testing Strategy

### Unit tests

- Upload state transitions and idempotent retries
- Path traversal and Windows name rejection
- Reparse point refusal
- JWT claim and owner validation
- Windows filename comparison
- Trash and restore conflicts
- Operation journal recovery decisions

### Integration tests

- Real temporary NTFS directories with SQLite
- Explorer-style create, rename, move, atomic replace, and delete
- Watcher overflow followed by generation reconciliation
- Forced termination between database and filesystem phases
- Interrupted upload and offset resume
- Range, ETag, and If-Range downloads
- Online database backup and restore reconciliation

### Security tests

- Parent traversal, UNC, alternate data streams, symlink, junction, and mount-point attacks
- Forged, expired, wrong-issuer, wrong-audience, and wrong-owner Access JWTs
- CSRF, Origin, and CORS enforcement
- HTML, SVG, XML, unknown MIME, and MIME-sniffing preview attacks

### Frontend end-to-end tests

- Project create, edit, archive, and delete
- Upload, interruption, resume, and completion
- File rename, move, copy, trash, and restore
- Preview allowlist and download fallback
- Desktop and mobile critical flows
- Offline and degraded-state behavior

### Installation tests

- Clean Windows installation
- Automatic startup after reboot
- Update and rollback
- Uninstall with user-data preservation

## 14. MVP Acceptance Criteria

- Cellar installs on the current PC as an automatically starting Windows service.
- Only the configured Cloudflare Access owner can reach the UI and API.
- The owner can create, edit, archive, delete, and restore projects.
- Projects display catalog metadata, file count, size, and cover.
- File and folder management works within the single storage root.
- Large uploads are resumable and have no application-level whole-file cap.
- Downloads and media previews stream and support byte ranges.
- Only allowlisted content is previewed inline.
- Explorer changes are detected and reconciled while preserving logical identity when NTFS identity is available.
- Reparse points and path escapes cannot expose files outside a project.
- Forced service termination does not expose incomplete uploads as complete files.
- Incomplete operations reconcile on restart.
- Install, API, integration, security, and critical frontend end-to-end tests pass.
- The completed service is installed and verified on the current PC.

## 15. Deferred Work

- Public or password-protected share links
- External upload requests
- Multi-user collaboration and role-based access
- Multi-device sync
- Multiple storage roots
- NAS and network storage
- Browser editing
- Office preview conversion
- Archive extraction
- Content indexing and semantic search
- Linux and macOS service installers
- Multi-PC control plane
- Independent preview or indexing workers

## 16. References

- [Cloudflare Tunnel](https://developers.cloudflare.com/tunnel/)
- [Cloudflare Tunnel origin parameters](https://developers.cloudflare.com/tunnel/advanced/origin-parameters/)
- [Cloudflare Access JWT validation](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/authorization-cookie/validating-json/)
- [Cloudflare request and response limits](https://developers.cloudflare.com/workers/platform/limits/)
- [SQLite write-ahead logging](https://www.sqlite.org/wal.html)
- [SQLite Online Backup API](https://sqlite.org/backup.html)

# Cellar Design Specification

**Date:** 2026-07-31

**Status:** Revision 2 — expert-reviewed; awaiting owner approval for implementation planning

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

- **Primary platform:** Windows 11 x64
- **Future platforms:** Linux and macOS through platform adapters
- **Owner model:** One configured owner
- **Remote ingress:** Cloudflare Tunnel
- **External authentication:** Cloudflare Access
- **Backend:** Rust, Axum, and Tokio
- **Frontend:** React and TypeScript
- **Metadata database:** SQLite
- **Deployment:** One Cellar Windows service
- **Storage:** One new or empty Cellar-managed directory on a fixed local NTFS volume
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
  -> TLS-authenticated https://127.0.0.1:9443 origin
  -> Cellar Windows service with exclusive port binding
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

The service listens only on `127.0.0.1:9443` for the authenticated HTTPS application origin and on a distinct `127.0.0.1:9444` health listener by default. Both fixed ports are configurable during installation. Cellar uses `SO_EXCLUSIVEADDRUSE` and fails closed if it cannot bind either configured port. No router port or public origin IP is opened.

The `cloudflared` to Cellar origin connection uses HTTPS. Installation creates a Cellar-specific origin certificate with a private key readable only by `SYSTEM`, `Administrators`, and `NT SERVICE\Cellar`. The emitted `cloudflared` configuration uses `caPool` and `originServerName`; `noTLSVerify` is always `false`. This prevents a different local process from impersonating Cellar and receiving Access tokens while Cellar is stopped.

Cloudflare Access permits only the configured owner's identity. The `cloudflared` route enables origin-side Access validation with:

- `access.required: true`
- The expected Cloudflare team name
- The Access application's audience tag

The authenticated origin exposes no anonymous route. The embedded UI, every API, download, preview, and server-sent event route applies the same Cellar authentication middleware. Minimal liveness and readiness endpoints exist only on the separate health listener, which is never included in the `cloudflared` route.

Cellar independently validates exactly one bounded-length `Cf-Access-Jwt-Assertion`:

- `alg=RS256`, header `typ=JWT`, and signature
- Exact issuer
- The configured audience in the token's audience array
- Expiration, not-before, issued-at, and a small configured clock skew
- Payload `type=app`
- The configured non-empty owner subject

Service-token identities and tokens with an empty owner subject are rejected.

Cellar caches Cloudflare JWKS keys with rotation support. Unknown-key refresh uses a timeout, response-size bound, single-flight request coalescing, and rate limit. Missing keys, stale keys that cannot be refreshed, malformed claims, or validation errors fail closed.

Authentication has two explicit modes:

- **Unenrolled:** Only `/owner/claim` is reachable on the authenticated origin. The middleware performs complete JWT cryptographic and claim validation but does not yet require a stored owner subject. The claim handler additionally requires an exact match to the configured bootstrap email, exact canonical `Origin`, and a 256-bit one-time claim code generated by the local administrator command. Only its hash is stored; it expires after 30 minutes and can be regenerated locally.
- **Enrolled:** `/owner/claim` is permanently disabled. Every authenticated route requires the JWT subject to exactly match the stored immutable owner subject.

After enrollment, all `POST`, `PUT`, `PATCH`, and `DELETE` requests require an unpredictable Access-session-bound anti-CSRF token in a custom header and exactly one `Origin` matching the configured canonical public HTTPS origin. The pre-enrollment `/owner/claim` route is the sole session-CSRF exception; its valid Access JWT, exact bootstrap-email match, exact canonical `Origin`, and expiring one-time claim code form its CSRF defense. Missing, `null`, duplicate, or mismatched origins are rejected. Mutation routes accept only their declared JSON or upload media type. `GET`, `HEAD`, and `OPTIONS` never mutate state. CORS headers are not emitted, credentialed cross-origin requests are not allowed, and cross-site fetch metadata is rejected as an additional defense.

After enrollment, authenticated `GET /api/v1/session` returns a random 256-bit CSRF token. Cellar stores only its hash, bound to owner subject and the current Access token issue/expiry window. The token rotates when the Access token changes, after eight hours, or after service restart. It is never placed in a cookie, URL, or log.

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

The Windows MVP accepts only a new or empty ordinary directory on a fixed local NTFS volume. Volume roots, network paths, removable filesystems, FAT/exFAT, EFS, OneDrive placeholders, reparse roots, Windows system directories, `Program Files`, and `ProgramData` are rejected. Adopting an existing non-empty root and changing roots are deferred.

Project directories use UUIDs rather than display names. At startup, Cellar opens the storage root with `FILE_FLAG_OPEN_REPARSE_POINT`, verifies that it is not a reparse point, records its volume serial and root file ID, and retains a trusted root handle for the service lifetime.

Security-sensitive operations open every path component relative to a trusted directory handle and reject reparse points. Validation and read, write, rename, and delete operate on the same final handle. The final volume, file identity, and resolved handle path must remain within the project root. New entries and renames are performed relative to a validated parent handle. Files with more than one hard link and case-sensitive directory subtrees are marked `unsupported` and are not mutated by Cellar.

Cellar runs as the fixed low-privilege Windows service identity `NT SERVICE\Cellar`, with access only to its program data and configured storage root.

The installer grants the configured interactive Windows owner and the Cellar service identity access to the project storage. It does not grant broad access to other local users.

### 4.4 Safe preview boundary

The MVP previews only an allowlist:

- PNG, JPEG, and WebP images
- MP3, WAV, Ogg audio, and MP4, WebM video through browser-native playback
- PDF
- UTF-8 plain text and common source-code extensions

| Category | Inline limit | Behavior |
|---|---:|---|
| Image | 50 MiB | Browser-native image display after extension and signature match |
| Audio/video | No whole-file cap | Range-streamed; unsupported codecs remain downloadable |
| PDF | 100 MiB | Sandboxed same-origin iframe without script permission |
| Text/code | 2 MiB | UTF-8 with replacement, escaped rendering, explicit truncation notice |

Inline type selection requires both an allowed extension and matching server-side file signature; client-supplied MIME is not trusted. HTML, SVG, XML, unknown types, mismatches, and executable formats are returned as `application/octet-stream` attachments.

Filenames in `Content-Disposition` strip CR, LF, and path separators and use an RFC 6266-compatible `filename*`. Every file response uses `X-Content-Type-Options: nosniff`, `Cache-Control: private, no-store`, and `Cross-Origin-Resource-Policy: same-origin`. PDF uses a sandboxed iframe. Text and code are decoded as UTF-8 with replacement, capped at 2 MiB, clearly marked when truncated, and inserted as text rather than HTML.

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
      `- <project-uuid>/<trash-item-uuid>/payload
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

At configuration and startup, Cellar verifies that every project, staging, and trash path has the same volume identity. The installer performs a create, write, flush, no-replace rename, and delete preflight as the actual service identity before readiness can succeed.

The UI displays and can copy each project's local path. Remote requests cannot launch Explorer on the host. UUID project directories must not be renamed manually.

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
  platform_kind
  platform_identity
  size
  mtime_filetime_100ns
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
- On Windows, the opaque platform identity contains the 8-byte volume serial and 16-byte `FILE_ID_128` used to maintain logical identity across Explorer rename and move operations.
- An atomic editor replacement at the same path with a new filesystem ID becomes a new revision of the same logical file.
- Ambiguous identity matches that are not hard links fall back to delete-plus-create behavior. A file with more than one hard link is always `unsupported` and is never mutated by Cellar.

Cellar preserves the exact on-disk filename. It does not normalize or rewrite names. Windows-compatible ordinal case-insensitive comparison uses a versioned `CompareStringOrdinal(..., TRUE)` custom SQLite collation registered on every connection. SQLite's built-in `NOCASE` is not used as a substitute for Windows filename comparison. A collation implementation change forces a full index rebuild and reconciliation.

Unsupported names are recorded as `unsupported` rather than deleted.

Project `version` is an optimistic-concurrency counter incremented by every project metadata mutation. File `revision` increments whenever the logical entry's content or location changes. Application timestamps serialize as RFC 3339 UTC; filesystem modification time preserves raw Windows 100-nanosecond FILETIME ticks. Ordering that must survive clock adjustment uses database sequence values rather than wall-clock time.

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
  pending_offset
  pending_length
  pending_digest
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
  payload_version
  payload
  error
  created_at
  updated_at
```

Operation states are:

```text
pending -> fs_applied -> complete
pending -> failed
fs_applied -> failed
```

`failed` is terminal only after the recovery decision table proves that automatic retry or completion is unsafe. All web-initiated mutations create an operation record before changing the filesystem. The versioned payload records source and destination logical IDs, comparison names, expected revision and filesystem identity, staging or trash path, and observed result identity. Startup recovery uses these facts to resume or reconcile incomplete operations.

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

Audit path snapshots are project-relative display values, never absolute host paths.

### 6.8 SQLite operation

SQLite uses:

- WAL mode
- `synchronous=FULL`
- Foreign keys enabled on every connection
- Busy timeout
- A small read pool and one controlled writer
- Versioned migrations
- SQLite Online Backup API for live backups

Restoring a backup always triggers a full filesystem reconciliation.

The migration contract is expand-only for the MVP. The previous release must be able to open the next release's schema until an update is confirmed healthy. Destructive or contract migrations are deferred.

The migrations follow this constraint skeleton; omitted descriptive columns retain the model above.

```sql
CREATE TABLE project (
  id TEXT PRIMARY KEY NOT NULL,
  name TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL CHECK (status IN ('active', 'archived')),
  version INTEGER NOT NULL CHECK (version >= 1),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  deleted_at TEXT
);

CREATE TABLE file_entry (
  id TEXT PRIMARY KEY NOT NULL,
  project_id TEXT NOT NULL,
  parent_id TEXT,
  exact_name TEXT NOT NULL COLLATE WINDOWS_ORDINAL_CI,
  kind TEXT NOT NULL CHECK (kind IN ('file', 'directory')),
  platform_kind TEXT NOT NULL,
  volume_serial BLOB,
  filesystem_file_id BLOB,
  size INTEGER NOT NULL CHECK (size >= 0),
  mtime_filetime_100ns INTEGER NOT NULL,
  hash BLOB,
  hash_state TEXT NOT NULL CHECK (
    hash_state IN ('unknown', 'queued', 'computing', 'ready', 'failed')
  ),
  state TEXT NOT NULL CHECK (
    state IN ('live', 'settling', 'missing', 'trashed', 'unsupported')
  ),
  revision INTEGER NOT NULL CHECK (revision >= 1),
  scan_generation INTEGER NOT NULL,
  observed_at TEXT NOT NULL,
  UNIQUE (id, project_id),
  FOREIGN KEY (project_id) REFERENCES project(id),
  FOREIGN KEY (parent_id, project_id)
    REFERENCES file_entry(id, project_id)
);

CREATE UNIQUE INDEX uq_file_root_name
  ON file_entry(project_id, exact_name)
  WHERE parent_id IS NULL AND state IN ('live', 'settling');

CREATE UNIQUE INDEX uq_file_child_name
  ON file_entry(project_id, parent_id, exact_name)
  WHERE parent_id IS NOT NULL AND state IN ('live', 'settling');

CREATE INDEX ix_file_platform_identity
  ON file_entry(project_id, platform_kind, volume_serial, filesystem_file_id);

CREATE TABLE project_cover (
  project_id TEXT PRIMARY KEY NOT NULL,
  file_entry_id TEXT NOT NULL,
  FOREIGN KEY (project_id) REFERENCES project(id),
  FOREIGN KEY (file_entry_id, project_id)
    REFERENCES file_entry(id, project_id)
);

CREATE TABLE upload_session (
  id TEXT PRIMARY KEY NOT NULL,
  project_id TEXT NOT NULL,
  destination_parent_id TEXT,
  destination_name TEXT NOT NULL COLLATE WINDOWS_ORDINAL_CI,
  expected_size INTEGER NOT NULL CHECK (expected_size >= 0),
  committed_offset INTEGER NOT NULL CHECK (committed_offset >= 0),
  expected_hash BLOB,
  pending_offset INTEGER,
  pending_length INTEGER,
  pending_digest BLOB,
  state TEXT NOT NULL CHECK (
    state IN ('created', 'uploading', 'verifying', 'committing',
              'complete', 'failed', 'cancelled')
  ),
  expires_at TEXT NOT NULL,
  CHECK (committed_offset <= expected_size),
  CHECK (expected_hash IS NULL OR length(expected_hash) = 32),
  CHECK (
    (pending_offset IS NULL AND pending_length IS NULL AND pending_digest IS NULL)
    OR
    (pending_offset = committed_offset
     AND pending_length > 0
     AND pending_offset + pending_length <= expected_size
     AND length(pending_digest) = 32)
  ),
  FOREIGN KEY (project_id) REFERENCES project(id),
  FOREIGN KEY (destination_parent_id, project_id)
    REFERENCES file_entry(id, project_id)
);

CREATE UNIQUE INDEX uq_upload_root_destination
  ON upload_session(project_id, destination_name)
  WHERE destination_parent_id IS NULL
    AND state IN ('created', 'uploading', 'verifying', 'committing');

CREATE UNIQUE INDEX uq_upload_child_destination
  ON upload_session(project_id, destination_parent_id, destination_name)
  WHERE destination_parent_id IS NOT NULL
    AND state IN ('created', 'uploading', 'verifying', 'committing');

CREATE TABLE operation (
  id TEXT PRIMARY KEY NOT NULL,
  project_id TEXT,
  kind TEXT NOT NULL,
  state TEXT NOT NULL CHECK (
    state IN ('pending', 'fs_applied', 'complete', 'failed')
  ),
  payload_version INTEGER NOT NULL CHECK (payload_version >= 1),
  payload TEXT NOT NULL,
  error TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  FOREIGN KEY (project_id) REFERENCES project(id)
);

CREATE TABLE trash_item (
  id TEXT PRIMARY KEY NOT NULL,
  project_id TEXT NOT NULL,
  root_entry_id TEXT,
  original_path_snapshot TEXT NOT NULL,
  storage_path TEXT NOT NULL UNIQUE,
  deleted_at TEXT NOT NULL,
  purge_after TEXT NOT NULL,
  state TEXT NOT NULL CHECK (
    state IN ('stored', 'restoring', 'restored', 'purging', 'purged', 'failed')
  ),
  FOREIGN KEY (project_id) REFERENCES project(id),
  FOREIGN KEY (root_entry_id, project_id)
    REFERENCES file_entry(id, project_id)
);

CREATE TABLE audit_event (
  sequence INTEGER PRIMARY KEY AUTOINCREMENT,
  event_id TEXT NOT NULL UNIQUE,
  operation_id TEXT,
  source TEXT NOT NULL CHECK (
    source IN ('web', 'explorer', 'reconcile', 'system')
  ),
  project_id TEXT,
  target_id TEXT,
  path_snapshot TEXT,
  action TEXT NOT NULL,
  result TEXT NOT NULL,
  occurred_at TEXT NOT NULL,
  details TEXT NOT NULL,
  FOREIGN KEY (operation_id) REFERENCES operation(id),
  FOREIGN KEY (project_id) REFERENCES project(id)
);
```

Cover references are explicitly cleared in the same transaction that changes a referenced entry from a live state. Audit events are never cascade-deleted with projects. Platform identity indexes are intentionally non-unique because hard links can share an identity.

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
- Each project has a monotonic `dirty_epoch`. Watcher overflow, watcher error, and events observed during a scan increment it.
- A root watcher overflow marks every project dirty because the affected project cannot be identified reliably.
- A scan may mark a project clean only when its starting and ending epoch match.
- Each scan uses a new `scan_generation`.
- Entries are incrementally upserted during the scan.
- Entries not observed are marked missing only after the complete scan succeeds and the dirty epoch remains unchanged.
- Repeated churn uses bounded retry and backoff; the project remains visibly dirty instead of entering an infinite rescan loop.

The filesystem remains the final authority. API requests revalidate the actual target instead of trusting cached metadata. ETags are concurrency hints, not locks.

Web mutations and reconciler database writes are serialized through one per-project queue. The filesystem can still change independently, so every operation revalidates the open handle's identity and expected revision. The first successful namespace operation wins; later work never overwrites the winner and returns `409` before reconciling. Cellar's own watcher events are correlated with operation IDs and file identities rather than blindly ignored.

A file becomes settled only when size and modification time are stable for the configured window and no incompatible writer share is observed.

Settled files with an unknown hash enter a low-priority bounded hashing queue. One hash worker runs by default and yields to active transfers. Hash failure records `hash_state=failed` and remains retryable. A download may stream before hashing completes but omits a strong ETag; strong ETag and ETag-based `If-Range` become available only after SHA-256 reaches `ready`.

## 8. Large File Transfer

"No application-level file size limit" means Cellar does not impose an arbitrary whole-file cap. Real limits are available disk space, filesystem limits, browser limits, and transport limits.

Cellar never uploads an entire large file in one request. The server advertises a maximum chunk size of 32 MiB, which remains below Cloudflare Free's currently documented maximum upload size. The client treats the advertised capability, rather than a Cloudflare plan assumption, as authoritative.

### Upload flow

```text
create session
  -> reserve destination
  -> upload sequential 32 MiB chunks
  -> validate offset, length, and SHA-256 digest
  -> write and FlushFileBuffers
  -> commit the new offset in SQLite
  -> validate final size and SHA-256
  -> close the staging handle
  -> record commit intent
  -> atomically no-replace rename within the same volume
  -> verify the resulting file identity
  -> update catalog and operation in one DB transaction
  -> mark complete
```

All sizes and offsets use the server range `0..i64::MAX`. JSON transports them as decimal strings to avoid JavaScript's integer precision limit.

Before writing a chunk, the session stores its pending offset, length, and digest. The server accepts only the next committed offset using compare-and-swap semantics. After a crash:

- A matching pending range already present in staging is digested and committed.
- Uncommitted trailing bytes without matching pending metadata are truncated to the committed offset.
- A staging file shorter than the committed offset marks the session failed.

A duplicate retry wholly within committed bytes succeeds only after the stored bytes match its digest, then returns the current committed offset. Overlapping, skipped, or mismatched chunks are rejected. The last chunk may be shorter than the advertised maximum. A zero-byte file uses a session with expected size zero and proceeds directly to finalize.

One file uploads sequentially in the MVP. Up to three different files upload concurrently by default, and the setting is configurable. A central reservation ledger accounts for the expected remaining bytes of active sessions, but this is admission control rather than a filesystem guarantee. Every write and commit rechecks actual free space. New work is rejected when free space would fall below a configurable safety reserve, which defaults to 5 GiB.

Incomplete sessions expire after seven days by default. Expiration is configurable, and cleanup is audited.

### Upload protocol

```text
POST   /api/v1/uploads
GET    /api/v1/uploads/{session-id}
PUT    /api/v1/uploads/{session-id}/chunk
POST   /api/v1/uploads/{session-id}/finalize
DELETE /api/v1/uploads/{session-id}
```

Session creation sends project and destination IDs plus `expectedSize` as a decimal string. The response returns the session ID, `committedOffset` as a decimal string, expiry, and `maxChunkSize`.

Each chunk uses `application/octet-stream`, exact `Content-Length`, `Upload-Offset`, and an RFC-compatible SHA-256 `Digest`. The raw request body cannot exceed the advertised 32 MiB maximum. Status returns the authoritative offset. Finalize is idempotent after successful completion. Cancel preserves enough journal state for safe cleanup.

### Download flow

Downloads and media preview support:

- Streaming without loading the whole file into memory
- `Range`
- `ETag`
- `If-Range`
- Resume after interruption
- Correct `Content-Disposition`

The MVP supports one byte range. Valid single, suffix, and open-ended ranges return `206` with exact `Content-Range` and `Content-Length`. A syntactically valid but unsatisfiable range returns `416` with `Content-Range: bytes */N`. A malformed range or multiple ranges are ignored and return the full `200` response. `HEAD` mirrors the corresponding response headers without a body. An `If-Range` mismatch returns the full `200`, not `416`.

When available, settled-file SHA-256 provides a strong ETag. Metadata is checked and streaming occurs through the same stable open handle. If cached identity or metadata no longer match, Cellar returns a conflict or settling response and schedules reconciliation rather than streaming a different object.

Cellar does not rely on Cloudflare CDN caching or an assumed unlimited response size.

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

Only the separate, non-proxied health listener exposes unauthenticated `/health/live` and `/health/ready`. Liveness returns no configuration data. Readiness fails while configuration, owner enrollment, migration, operation recovery, storage preflight, or reconciliation is incomplete.

The service enters a degraded state rather than exiting when the storage root is temporarily unavailable. Readiness and the authenticated UI expose a stable reason code; mutating operations remain disabled until recovery.

Every request receives a request ID linked to structured logs and audit events.

Directory lists use cursor pagination with a default page size of 100 and maximum of 500. Stable ordering uses the Windows comparison key followed by logical entry ID. All overwrite behavior is forbidden in the MVP. Case-only rename uses a server-generated temporary name and two no-replace renames inside one journaled operation.

Server-sent events notify the frontend about uploads, reconciliation, catalog changes, degraded state, and recovery. Events have a monotonic sequence ID. The server retains a bounded replay window; if `Last-Event-ID` is too old, it sends a reset event and the client invalidates queries and obtains a fresh snapshot version. Reconnect uses exponential backoff and falls back to low-frequency polling.

### Resource limits

Whole-file size has no arbitrary application cap, but every resource is bounded:

- Request headers: 64 KiB
- JSON body: 1 MiB
- Raw upload chunk: advertised maximum, default 32 MiB
- Active upload sessions: 8
- Concurrent file uploads: 3
- Open downloads or previews: 8
- SSE clients: 4
- Hash workers: 1 by default
- Reconciliation: one project at a time and one scan per project
- Bounded queues for audit, hashing, watcher events, and SSE replay

Limits are configurable within safe ranges. Overload returns `413` or `429` with `Retry-After`. Disconnects propagate cancellation. Chunk requests have a 10-minute idle timeout. Hashing, scanning, and JWKS refresh use bounded work pools and single-flight coalescing where appropriate.

### Observability

Application and updater logs are structured JSON with timestamp, level, event code, version, request ID, operation ID, and sanitized context. Logs rotate at 20 MiB with ten retained files by default. JWTs, cookies, CSRF values, tunnel credentials, request bodies, owner email, and absolute user paths are never logged. Audit retention defaults to 180 days and is configurable. Startup, migration, storage preflight, reconciliation failure, degraded state, and fatal service errors are also written to Windows Event Log.

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
- Automatic interrupted-session resume while the source `File` remains available in the active page
- After reload or browser restart, incomplete sessions remain visible and the owner reselects the source file; name, size, modification time, and sampled content are verified before resume
- Actionable error messages

Download resume means correct server `Range` support and browser-native download behavior. The MVP does not promise a custom cross-browser download manager.

### Responsive behavior

Desktop uses a compact sidebar, project content area, and preview panel. Mobile collapses navigation and exposes primary file actions in a reachable bottom action surface.

Destructive actions require explicit confirmation. Connection loss places the UI in a visible read-only state until synchronization completes.

The visual direction uses restrained wine and slate colors, deliberate typography and spacing, subtle motion, and content-led hierarchy.

The accessibility target is WCAG 2.2 AA for critical flows. Project creation, browsing, upload selection, download, rename, move, trash, and restore must work with keyboard only. The UI supports 200% zoom, visible focus, reduced motion, 44 CSS-pixel touch targets, screen-reader transfer announcements, modal focus trapping and restoration, and a file-picker alternative to drag-and-drop.

Supported clients are the current and previous major desktop versions of Chrome, Edge, and Firefox, current Safari on iOS, and current Chrome on Android. Automated coverage uses Chromium, Firefox, and WebKit, with mobile viewport and touch emulation plus critical-device smoke testing.

## 11. Codebase Structure

```text
cellar/
|- crates/
|  |- cellar-service/
|  |- cellar-config/
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

`cellar-service` is the executable and composition root. `cellar-core` contains platform-neutral domain types and ports. API, database, and platform crates depend inward on core contracts; core does not depend on Axum, SQLx, or Windows. Physical file identity is opaque in core and interpreted only by a platform adapter.

### Web

- React and TypeScript
- Vite
- TanStack Router and Query
- Accessible headless primitives
- CSS variables and a utility-based styling layer

The production web build is embedded in `cellar.exe`. Node.js is needed for development and build only, not on the installed machine.

## 12. Windows Installation and Lifecycle

### Install

The supported host is Windows 11 x64 on a fixed local NTFS volume. The administrator-run `install.ps1` is idempotent and:

1. Verifies or builds a release package.
2. Installs `cellar.exe` under `C:\Program Files\Cellar`.
3. Creates `C:\ProgramData\Cellar` directories.
4. Runs `cellar configure` with the canonical external HTTPS origin, Cloudflare team domain, audience tag, bootstrap owner email, storage root, origin port, and separate health port.
5. Generates the local origin certificate and emits the exact `cloudflared` route fragment without storing or changing tunnel credentials.
6. Registers an automatically starting Windows service as `NT SERVICE\Cellar`.
7. Applies least-privilege ACLs to program data, the origin key, service control, and the selected storage root.
8. Performs the NTFS storage preflight as the service identity.
9. Starts the service and verifies loopback liveness and readiness.

Origin TLS uses a Cellar-local CA with a five-year validity and a one-year `cellar.local` leaf certificate. The leaf renews 30 days before expiry without changing `caPool`. Approaching CA expiry places readiness in a warning state and requires an administrator-run CA rotation command that emits an updated Cloudflare route fragment. Failed renewal retains the last valid certificate and records an Event Log error.

Installation verifies that ordinary users and the service identity cannot write `C:\Program Files\Cellar`, that only the documented principals can read the origin private keys and ProgramData, and that the external `cloudflared` credential location is not broadly readable.

Before owner enrollment, readiness reports `owner_enrollment_required` and every file route remains closed. The configured bootstrap email may access one claim endpoint only after a valid Cloudflare Access JWT. A successful exact-email match stores the immutable Access subject, clears the bootstrap email, permanently disables the claim endpoint, and enables normal readiness. Later email changes do not change owner identity; owner replacement requires an administrator-run local command.

The generated `cloudflared` fragment includes the public hostname route, `originRequest.access.required`, `teamName`, `audTag`, `caPool`, `originServerName`, and `noTLSVerify: false`.

The Windows service accepts preshutdown notification, has a 180-second preshutdown budget and 60-second normal stop target, stops accepting mutations, flushes journal state, and then exits. SCM recovery restarts the service after 30 seconds up to three times before leaving it stopped and recording a Windows Event Log error.

### Update

The updater is administrator-only, idempotent, and records a durable phase marker. It:

1. Quiesces new mutations.
2. Creates a recovery set containing the executable, SQLite online backup, configuration, schema version, storage volume identity, and integrity result.
3. Gracefully stops the service.
4. Atomically installs the new executable.
5. Applies expand-only database migrations.
6. Starts the service with mutations disabled until migration, operation recovery, storage preflight, and health checks pass.
7. Confirms the update and deletes the phase marker.
8. On failure, stops the new service and restores the complete executable, database, and configuration recovery set before starting the old version.

The updater recovers or safely resumes after interruption at every phase. It retains the latest three update recovery sets. Destructive or contract migrations are not allowed in the MVP. Release packages require a checked SHA-256 manifest. Authenticode signing is deferred for the personal-installation MVP and becomes mandatory before third-party distribution.

### Uninstall

Uninstall removes the Windows service and program files. It preserves `ProgramData`, SQLite data, backups, and project files by default. `-RemoveProgramData` and `-RemoveProjectFiles` are separate options. Project-file removal requires storage-root revalidation and a second explicit confirmation.

### Cloudflare

The MVP uses a remotely managed named Cloudflare Tunnel. The owner creates the tunnel and published application route in the Cloudflare dashboard, applies the Cellar-emitted origin settings there, and installs `cloudflared` as a separate Windows service using Cloudflare's tunnel token. Its tunnel token is not stored in Cellar configuration.

Cellar diagnostics may inspect only local service/process state and perform an external-origin smoke test. Cellar does not read Cloudflare credentials or modify the tunnel. Installation remains not-ready until the externally applied route passes TLS, Access, owner-enrollment, and origin smoke tests.

## 13. Normative Storage and Recovery Contracts

### 13.1 Global invariants

- A user-visible destination is never partially populated.
- A destination is never overwritten implicitly.
- An operation may complete once or fail visibly, but retry and recovery must not apply it twice.
- The filesystem namespace operation is authoritative; SQLite records and explains its result.
- Every web mutation is serialized with reconciler writes for the project, validates expected revision and open-handle identity, and uses an atomic no-replace destination operation.
- Staging and trash paths use operation UUIDs, are protected by ACL, and are never derived from user-visible path snapshots.
- Purge, expiry, and project deletion obtain a lease and do not race an active upload or operation.
- Copy writes to hidden same-volume staging, flushes, verifies, and publishes with no-replace rename.
- Download and copy open a stable handle and reject a settling file or incompatible writer.

### 13.2 Namespace recovery decision table

For rename, move, trash, restore, and project deletion, the operation payload records expected source identity and intended destination identity.

| Source | Destination | Recovery action |
|---|---|---|
| Expected source exists | Destination absent | Operation has not applied; retry after validation or fail without changing the source |
| Source absent | Expected destination exists | Filesystem phase applied; verify identity and complete the catalog transaction |
| Source exists | Destination exists | Conflict; preserve both, mark failed, return `409`, and reconcile |
| Source absent | Destination absent | Mark failed or missing; never invent success; reconcile the containing namespace |

An unexpected identity at either path is treated as another actor's object and is never modified automatically.

### 13.3 Copy, create, and case-only rename recovery

Copy payload records the stable source identity, staging identity, expected-absent destination, and expected result identity.

| Copy state | Recovery action |
|---|---|
| Expected staging exists; destination absent | Resume or revalidate copy, then publish with no-replace rename |
| Staging absent; destination has expected result identity | Complete the catalog transaction |
| Destination has an unexpected identity | Preserve every object, fail with conflict, and reconcile |
| Staging and destination absent | Retry from the still-matching source or fail without changing it |

Directory and project creation record the expected parent identity, comparison name, and result identity. An absent destination is safe to retry with no-replace create; a destination with the recorded result identity completes the catalog; any other destination is a preserved conflict.

A case-only rename uses a server-generated temporary name and records source, temporary, and final identities:

| Observed identity location | Recovery action |
|---|---|
| Source only | Retry source-to-temporary no-replace rename |
| Temporary only | Retry temporary-to-final no-replace rename |
| Final only | Complete the catalog transaction |
| Identity at multiple locations or unexpected occupant | Preserve all entries, fail with conflict, and reconcile |

### 13.4 Upload recovery

Chunk processing order is:

```text
record pending chunk metadata
-> write exact range
-> FlushFileBuffers
-> compare-and-swap committed_offset and clear pending metadata
```

Final commit order is:

```text
verify final size and SHA-256
-> flush and close staging handle
-> record commit intent
-> atomic no-replace rename
-> open and verify result identity
-> catalog update and operation completion in one SQLite transaction
```

Recovery compares staging length, committed offset, pending metadata, digest, final path, and result identity. It advances a fully written matching pending chunk, truncates uncommitted trailing data, fails when durable data is shorter than the committed offset, and never exposes staging as complete.

### 13.5 Delete and project recovery

Web deletion moves an item to `.cellar/trash/<project-id>/<trash-item-id>/payload`. Restore uses the recorded original logical parent and name but never overwrites a current object.

Project deletion:

1. Blocks new project mutations.
2. Requests cancellation of active uploads.
3. Waits for active handles to drain within the stop policy.
4. Moves the complete project directory to its unique trash item.
5. Commits deleted state only after identity verification.

Project restore reverses the move only when the original UUID destination is absent. It restores project metadata and cover references from the retained database record. Conflicts return `409`.

### 13.6 Backup and restore

Backup sets protect the SQLite catalog, audit and operation state, and a separate copy of `config.toml`; they are not content backups. Each backup manifest records schema version, configured storage-root identity, creation time, database integrity result, application version, and configuration digest.

Administrator-only `cellar backup create`, `cellar backup list`, and `cellar backup restore <id>` commands implement the lifecycle. Manual catalog backups retain seven sets by default; updater recovery sets have their separate three-set retention.

Restore order is:

```text
quiesce mutations
-> preserve current database
-> validate backup in a temporary location
-> replace database
-> apply compatible migrations
-> perform full storage reconciliation
-> restore readiness
```

If validation, migration, or reconciliation initialization fails, Cellar restores the preserved current database. Project UUID directories created after the backup are imported as active projects named `Recovered <short-uuid>` with an empty description and no cover rather than hidden or deleted.

## 14. Testing Strategy

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
- Forced termination at every journal and updater phase
- Interrupted upload and offset resume
- Destination creation by Explorer between validation and no-replace commit
- Empty, suffix, open-ended, malformed, unsatisfiable, multi-range, and If-Range mismatch requests
- Range, ETag, If-Range, and HEAD downloads through one stable handle
- Case-only rename, Unicode names, long paths, hard links, case-sensitive subtrees, ACL denial, antivirus-style sharing violations, disk full, and database busy
- Online database backup and restore reconciliation
- Recovery of project directories created after the restored catalog backup

### Security tests

- Parent traversal, UNC, alternate data streams, symlink, junction, and mount-point attacks
- Forged, expired, wrong-issuer, wrong-audience, and wrong-owner Access JWTs
- Duplicate and oversized JWT headers, service tokens, unknown-key refresh storms, and local origin impersonation
- CSRF, Origin, and CORS enforcement
- HTML, SVG, XML, unknown MIME, and MIME-sniffing preview attacks
- Header injection through filenames and verification that logs contain no JWT, cookie, CSRF value, credential, or absolute private path

### Frontend end-to-end tests

- Project create, edit, archive, and delete
- Upload, interruption, resume, and completion
- File rename, move, copy, trash, and restore
- Preview allowlist and download fallback
- Desktop and mobile critical flows
- Offline and degraded-state behavior
- Keyboard-only critical flows, 200% zoom, reduced motion, screen-reader transfer announcements, automated accessibility checks, and 44-pixel mobile touch targets

### Installation tests

- Clean disposable Windows 11 x64 VM installation through owner enrollment and real Cloudflare Access
- Automatic startup after reboot
- `N-1` to `N` update and rollback at every durable update phase
- Storage-root allow and deny matrix plus service-identity preflight
- Service stop, preshutdown, crash recovery, and pending-reboot behavior
- Idempotent installer and updater reruns
- Uninstall with byte-identical user-data preservation

### Performance and scale tests

The initial reference PC is Windows 11 x64 with an AMD Ryzen 5 5600X, 16 GiB RAM, and a local 2 TB NTFS SATA hard drive. Results record hardware and storage details so later reference environments can define separate baselines.

- A project fixture containing 100,000 entries uses pagination and completes initial reconciliation on the reference PC within five minutes.
- Normal settled Explorer changes appear within five seconds.
- Watcher overflow visibly marks the project dirty and recovers within the next successful scan cycle.
- A generated 10 GiB upload crosses the ingress through multiple chunks, survives interruption and service restart, and finishes with the source SHA-256.
- Offset and schema tests cover values beyond JavaScript's exact integer range without requiring a physically enormous file.
- Peak service memory remains bounded by configured concurrency rather than file size.

## 15. MVP Acceptance Criteria

- Cellar installs on Windows 11 x64 as an automatically starting service under `NT SERVICE\Cellar`.
- Clean installation, non-interactive configuration, origin TLS, Cloudflare route setup, one-time owner enrollment, and readiness complete without editing application files manually.
- Only the configured Cloudflare Access owner subject can reach the UI, API, downloads, previews, and SSE.
- Non-owner, malformed JWT, service token, missing JWT, and local origin impersonation requests are rejected.
- Allowed and denied storage-root test matrices produce the documented result, and the service-identity NTFS preflight passes.
- The owner can create, edit, archive, delete, and restore projects.
- Projects display catalog metadata, file count, size, and cover.
- File and folder management works within the single storage root.
- A 10 GiB reference upload is interrupted, resumed, survives service restart, and completes with a matching SHA-256; protocol offset tests cover the full `i64` range.
- Single-range, suffix, open-ended, HEAD, invalid, unsatisfiable, and If-Range behavior passes the documented HTTP contract.
- Only allowlisted content is previewed inline.
- Normal Explorer changes settle and appear within five seconds; overflow recovery completes within the next successful scan cycle.
- Explorer changes preserve logical identity when a unique NTFS identity is available and follow documented fallback rules otherwise.
- Reparse points and path escapes cannot expose files outside a project.
- Forced termination at every mutation journal boundary produces no overwrite, duplicate application, partial publication, or silent source loss.
- `N-1` to `N` update succeeds, and interruption at every durable updater phase restores or safely resumes the complete recovery set.
- Default uninstall preserves ProgramData and project files byte-for-byte.
- Logs rotate and contain no JWT, cookie, CSRF value, tunnel credential, or absolute private path.
- Keyboard-only, mobile viewport, 200% zoom, reduced-motion, and automated accessibility checks pass for critical flows.
- Install, API, integration, security, and critical frontend end-to-end tests pass.
- The completed service is installed and verified on the current PC.

## 16. Delivery Phases

The approved v1 scope is delivered through ordered milestones rather than as one undifferentiated implementation:

1. **Security and service foundation:** configuration CLI, origin TLS, Cloudflare authentication, owner enrollment, Windows service, storage preflight, SQLite migrations, logs, and installer.
2. **Core project files:** project catalog, folder browsing, cursor pagination, resumable upload, ranged download, rename, move, trash, restore, operation journal, and crash recovery.
3. **Explorer reconciliation:** NTFS identity, watcher, dirty epochs, generation scans, conflict behavior, and recovered-project handling.
4. **Preview and product experience:** safe image and text preview first, then PDF and browser-native media, responsive project views, transfers, accessibility, recent items, and cover management.
5. **Release lifecycle:** updater, rollback, backup/restore CLI, uninstall, VM test matrix, Cloudflare end-to-end verification, and installation on the current PC.

Every milestone must pass its relevant tests before the next begins. All five milestones are required for v1 acceptance; this sequencing does not remove approved features.

## 17. Deferred Work

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
- Existing non-empty storage-root adoption
- Storage-root relocation
- FAT, exFAT, removable, EFS, OneDrive, network, or case-sensitive directory support
- Authenticode signing before third-party distribution

## 18. References

- [Cloudflare Tunnel](https://developers.cloudflare.com/tunnel/)
- [Cloudflare Tunnel origin parameters](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/configure-tunnels/origin-parameters/)
- [Cloudflare Access JWT validation](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/authorization-cookie/validating-json/)
- [Cloudflare upload limits and default cache behavior](https://developers.cloudflare.com/cache/concepts/default-cache-behavior/)
- [Cloudflare connection limits](https://developers.cloudflare.com/fundamentals/reference/connection-limits/)
- [SQLite write-ahead logging](https://www.sqlite.org/wal.html)
- [SQLite Online Backup API](https://sqlite.org/backup.html)
- [Microsoft exclusive socket use](https://learn.microsoft.com/en-us/windows/win32/winsock/using-so-reuseaddr-and-so-exclusiveaddruse)
- [Microsoft reparse-point file operations](https://learn.microsoft.com/en-us/windows/win32/fileio/reparse-points-and-file-operations)
- [Microsoft final path by handle](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getfinalpathnamebyhandlew)
- [Microsoft service preshutdown contract](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/ns-winsvc-service_preshutdown_info)
- [OWASP CSRF prevention](https://cheatsheetseries.owasp.org/cheatsheets/Cross-Site_Request_Forgery_Prevention_Cheat_Sheet.html)
- [OWASP file upload guidance](https://cheatsheetseries.owasp.org/cheatsheets/File_Upload_Cheat_Sheet.html)
- [RFC 6266 Content-Disposition](https://www.rfc-editor.org/rfc/rfc6266)

# Cellar Rewrite MVP Design

**Date:** 2026-08-04

**Status:** Approved for implementation planning

**Branch:** `codex/rewrite-mvp`

**Preserved baseline:** `develop` at `403689b`

## 1. Purpose

Cellar is a single-owner web application for remotely storing and retrieving project files from one Windows PC. The rewrite starts with one complete, usable path instead of building the full long-term storage architecture first:

1. Sign in through Cloudflare Access.
2. Create a project.
3. Upload a file of any application-supported size through resumable chunks.
4. See the file in that project.
5. Download the same bytes, including range resume.

The existing `develop` branch remains intact. The rewrite is implemented independently on `codex/rewrite-mvp`; existing source code may be consulted for lessons but is not copied by default.

## 2. MVP Scope

### Included

- One configured owner
- Cloudflare Tunnel and Cloudflare Access ingress
- Origin-side Cloudflare Access JWT validation
- Project create and list
- Flat file list per project
- Resumable, sequential 32 MiB chunk upload
- Upload status after browser or service restart
- Atomic no-overwrite publication
- Single-range and full-file download
- React web UI embedded in the Rust server
- Windows-first local filesystem storage
- SQLite persistence for projects and upload sessions

### Excluded

- Folders and breadcrumb navigation
- Rename, move, copy, trash, and restore
- File catalog duplication in SQLite
- Explorer change tracking
- Generic operation journal or reconciliation engine
- File previews, thumbnails, covers, tags, and search indexing
- Public links, multiple users, and role management
- Full-file SHA-256 verification
- Installer, updater, and automatic `cloudflared` provisioning

These exclusions are deliberate MVP boundaries, not implicit future commitments.

## 3. Architecture

Cellar is one Rust process plus the separately managed `cloudflared` process.

```text
Remote browser
  -> Cloudflare Access
  -> Cloudflare Tunnel
  -> cloudflared on the Windows PC
  -> http://127.0.0.1:<cellar-port>
  -> Cellar Rust server
       |- auth
       |- projects
       |- uploads
       |- files
       `- app/config
            |- SQLite
            `- local NTFS storage root
```

The Rust server uses Axum and Tokio. The frontend uses React, TypeScript, and Vite. The production frontend build is embedded into the Rust executable and served by the same origin as the API.

The server binds only to loopback. Plain HTTP is acceptable on this same-PC loopback hop because every API request also requires a valid Cloudflare Access assertion and the port is not exposed to the network.

The implementation stays a small modular monolith. Modules expose narrow service interfaces but do not create separate processes, message buses, or microservices.

## 4. Storage and Data Model

### Filesystem layout

```text
<storage-root>/
|- projects/
|  `- <project-uuid>/
|     `- files/
|        `- <validated-file-name>
`- .cellar/
   `- uploads/
      `- <upload-uuid>.part
```

The storage root must be an existing local directory on one fixed NTFS volume. Project and upload directories are created beneath that root. Temporary uploads and final project files stay on the same volume so final publication can use an atomic rename.

Remote clients never submit an absolute or relative path. They submit exactly one filename component. Cellar rejects separators, `.` and `..`, Windows reserved device names, alternate data stream syntax, trailing dots or spaces, control characters, and names exceeding the Windows UTF-16 component limit.

### SQLite tables

```text
project
  id
  name
  created_at

upload_session
  id
  project_id
  file_name
  expected_size
  committed_offset
  state            uploading | finalizing | complete | failed
  created_at
  updated_at
```

File metadata is not duplicated in SQLite. File listing reads the actual project directory and returns regular files only. The first MVP does not present folders, reparse points, or other special entries.

Sizes and offsets are signed 64-bit integers in Rust and SQLite and decimal strings in JSON so browser clients do not lose precision.

## 5. HTTP Contract

All routes are versioned under `/api/v1`.

### Projects

```text
GET  /api/v1/projects
POST /api/v1/projects
```

Project creation accepts a bounded display name. The server generates the project UUID and creates its storage directory before returning success. A failed directory creation does not leave a visible project row.

The server creates the UUID directory with no-replace semantics, then inserts the project row. If the database insert fails, it removes only the exact empty directory it just created. If that cleanup also fails, the API returns `503`; the unreferenced UUID directory remains invisible and startup reports it for manual cleanup instead of importing it as a project.

### Uploads

```text
POST /api/v1/projects/{projectId}/uploads
GET  /api/v1/uploads/{uploadId}
PUT  /api/v1/uploads/{uploadId}/chunk
POST /api/v1/uploads/{uploadId}/complete
```

Session creation accepts `fileName` and `expectedSize` as a decimal string. It rejects a final destination that already exists.

Chunk requests use `application/octet-stream`, exact `Content-Length`, and `Upload-Offset`. A chunk is at most 32 MiB. Chunks are sequential:

- The requested offset must equal the durable committed offset.
- A repeated chunk at an earlier offset returns the authoritative offset without appending duplicate data.
- A future or overlapping offset returns `409`.
- The server flushes the chunk before advancing the SQLite offset.
- The response always includes the authoritative committed offset.

After a browser reload, the user must select the same local file again because browsers do not preserve `File` handles across sessions. The client verifies the selected filename and size against the saved upload session, seeks to the server's committed offset, and resumes from there.

Completion requires the committed offset and staging file length to equal the expected size. Cellar sets the session to `finalizing`, flushes the staging file, then publishes with a same-volume no-overwrite rename. It marks the session complete only after the final file is verified.

On startup, only upload sessions need recovery. For each nonterminal session, Cellar compares the SQLite offset, staging length, and final destination:

- Matching staging data remains resumable.
- Uncommitted trailing staging bytes are truncated to the committed offset.
- A shorter staging file fails the session.
- A `finalizing` session with the expected final file and no staging file becomes complete.
- A conflicting final file is preserved and the session fails.

This is a focused upload state machine, not a generic mutation journal.

### Files

```text
GET  /api/v1/projects/{projectId}/files
GET  /api/v1/projects/{projectId}/files/{fileName}
HEAD /api/v1/projects/{projectId}/files/{fileName}
```

The list response contains filename, decimal byte length, and modification time. Download supports full responses and one RFC-compatible byte range. Unsatisfiable ranges return `416`; multiple ranges are not supported in the MVP. The response uses attachment disposition and does not infer an inline preview policy.

## 6. Authentication and Request Security

Cloudflare Access protects the public hostname. The origin additionally validates the `Cf-Access-Jwt-Assertion` header according to Cloudflare's documented origin-validation flow:

- RS256 signature against the team JWKS
- expected issuer
- expected application audience
- expiration and not-before time
- application token type
- configured owner email after ASCII lowercase normalization

JWKS values are cached and refreshed when a token references an unknown key. Validation fails closed when keys cannot be obtained or claims do not match. Cloudflare documents the header and rotating JWKS endpoint at <https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/authorization-cookie/validating-json/>.

CORS is disabled. Unsafe API methods require an `Origin` header exactly equal to the configured external origin. Static frontend assets can load on loopback for development, but every API route is authenticated unless an explicit development-only bypass is enabled in a debug build. Production configuration rejects that bypass.

## 7. Cloudflare Constraint

Cloudflare Free currently limits a request body to 100 MB. Therefore the browser always uses 32 MiB chunks and never sends a whole large file in one request. Cloudflare documents the current account-plan limit at <https://developers.cloudflare.com/workers/platform/limits/>.

`cloudflared` remains an independently installed and configured Windows process for the first MVP. Cellar configuration records only the external origin, team domain, application audience, owner email, local port, database path, and storage root. Tunnel credentials are never stored in the Cellar database.

## 8. Web Experience

The interface is a simple productivity web application, not a branded landing page.

- No Cellar logo, symbol, hero, or marketing header
- White and light-gray surfaces with one blue action color
- A narrow sidebar headed `Projects`
- The sidebar contains only user-created project names, plus Uploads and Settings
- The initial state contains no sample projects or sample files
- The empty state has one `Create project` action
- A project view shows the project name, file count and size, upload action, and a plain file table
- Ongoing uploads appear in a small persistent panel with progress and resume state
- On narrow screens, the sidebar becomes a project selector

The selected design is the brandless sidebar layout recorded by the visual brainstorming session. Example names such as `Photos` are mock content only and are not seeded by the product.

## 9. Error Contract

Errors use a small JSON envelope with a stable machine-readable code and request ID.

```text
400  invalid filename, size, offset, or request body
401  missing, expired, or invalid Access JWT
403  wrong owner, audience, issuer, token type, or Origin
404  project, upload session, or file not found
409  destination exists, offset conflict, or incompatible upload state
413  chunk exceeds 32 MiB
416  unsatisfiable download range
507  insufficient disk space
503  SQLite, storage root, or JWKS temporarily unavailable
```

Responses and logs never contain JWTs, owner email, tunnel credentials, request bodies, or absolute host paths.

## 10. Verification and Acceptance

### Backend

- Unit tests for filename rules, decimal size parsing, range parsing, and upload state decisions
- HTTP contract tests for authentication, Origin checks, projects, chunks, conflicts, and ranges
- Integration tests using a real temporary SQLite database and filesystem root
- Restart tests at each upload completion boundary
- Tests proving no overwrite and no duplicate chunk append

### Frontend

- Component tests for empty state, project creation, file table, and upload progress
- Upload client tests for 32 MiB chunking, authoritative offset handling, retry, and resume
- Responsive tests for desktop sidebar and mobile project selector
- Browser test for the complete project-create, upload, list, and download flow

### MVP acceptance flow

1. Reach the configured public hostname and complete Cloudflare Access login.
2. Create a project from an empty installation.
3. Upload a file larger than 100 MB in 32 MiB chunks.
4. Interrupt the upload, restart the service, and resume at the durable offset.
5. See the published file in the real directory-backed list.
6. Download the file with a full request and a resumed range request.
7. Verify that downloaded bytes match the source.
8. Confirm that a duplicate filename and invalid offset cannot overwrite or corrupt data.
9. Complete the critical flow on desktop and a mobile viewport.

## 11. Rewrite Delivery Rule

Implementation planning begins from this document. The first implementation commit removes the previous application structure from the rewrite branch and creates only the modules required by this MVP. The preserved `develop` branch is not rewritten or force-pushed. Integration back to `develop` is a later explicit user decision after the rewrite passes its acceptance flow.

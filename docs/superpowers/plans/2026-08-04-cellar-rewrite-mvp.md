# Cellar Rewrite MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a small, Windows-first private file hub where one Cloudflare Access-authenticated owner can create projects, upload large files resumably, see the real files in each project, and download them safely.

**Architecture:** Replace the legacy multi-crate implementation with one Axum application at the repository root. The server owns SQLite metadata and an app-controlled NTFS directory, validates Cloudflare Access at the application boundary, serves a compiled React application from the same loopback origin, and uses a separately managed `cloudflared` process for public ingress. Upload writes are sequential, serialized per upload, flushed before offsets are committed, and recovered conservatively on startup.

**Tech Stack:** Rust 1.93, Axum 0.8, Tokio, SQLx/SQLite, jsonwebtoken/reqwest, rust-embed, React 19, TypeScript 6, Vite 8, Vitest/Testing Library, Cloudflare Tunnel + Access.

---

## Target Repository Map

The rewrite keeps the approved design document and replaces the old runtime code with this deliberately small tree:

```text
cellar-rewrite/
├── Cargo.toml
├── Cargo.lock
├── README.md
├── config.example.toml
├── rust-toolchain.toml
├── migrations/
│   └── 0001_initial.sql
├── scripts/
│   ├── check.ps1
│   └── run-dev.ps1
├── src/
│   ├── main.rs                 # process startup and shutdown
│   ├── lib.rs                  # public test surface
│   ├── app.rs                  # router, middleware, shared state
│   ├── auth.rs                 # Cloudflare JWT/JWKS verification
│   ├── config.rs               # TOML config and validation
│   ├── db.rs                   # pool, migration, row repositories
│   ├── error.rs                # stable JSON error contract
│   ├── files.rs                # listing, HEAD, full/range GET
│   ├── projects.rs             # project API and directory creation
│   ├── storage.rs              # safe names, paths, flush, no-replace move
│   ├── uploads.rs              # upload state machine and endpoints
│   └── web.rs                  # embedded SPA/static responses
├── tests/
│   ├── common/mod.rs           # temporary app fixture and fake identity
│   ├── auth_boundary.rs
│   ├── projects_api.rs
│   ├── uploads_api.rs
│   ├── upload_recovery.rs
│   └── downloads_api.rs
└── web/
    ├── index.html
    ├── package.json
    ├── package-lock.json
    ├── tsconfig*.json
    ├── vite.config.ts
    └── src/
        ├── main.tsx
        ├── app.tsx
        ├── api.ts
        ├── types.ts
        ├── upload-client.ts
        ├── styles.css
        ├── test/setup.ts
        └── components/
            ├── app-shell.tsx
            ├── empty-state.tsx
            ├── file-table.tsx
            ├── project-create-dialog.tsx
            └── upload-panel.tsx
```

### Ownership boundaries

- `storage.rs` is the only module allowed to construct disk paths or mutate files.
- `db.rs` is the only module allowed to issue SQL.
- `auth.rs` returns a normalized `OwnerIdentity`; handlers never parse Cloudflare headers themselves.
- `uploads.rs` owns the upload state machine and calls storage before advancing committed database offsets.
- `files.rs` reads the actual project directory; SQLite is not a file catalog.
- `web/src/api.ts` is the only general JSON HTTP client; `upload-client.ts` alone sends chunk bodies.

## API and Persistence Contracts

All JSON sizes and offsets are decimal strings. Errors always have this shape:

```json
{
  "error": {
    "code": "upload_offset_conflict",
    "message": "The upload offset does not match the committed offset.",
    "requestId": "019...",
    "details": { "expectedOffset": "33554432" }
  }
}
```

SQLite starts with only two domain tables:

```sql
CREATE TABLE project (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE upload_session (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES project(id) ON DELETE CASCADE,
  file_name TEXT NOT NULL,
  total_size INTEGER NOT NULL CHECK (total_size >= 0),
  committed_offset INTEGER NOT NULL DEFAULT 0 CHECK (committed_offset >= 0),
  state TEXT NOT NULL CHECK (state IN ('active', 'finalizing', 'complete', 'failed')),
  failure_reason TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
```

The stable API is:

```text
GET    /api/v1/projects
POST   /api/v1/projects
GET    /api/v1/projects/{projectId}/files
GET    /api/v1/projects/{projectId}/files/{fileName}
HEAD   /api/v1/projects/{projectId}/files/{fileName}
POST   /api/v1/projects/{projectId}/uploads
GET    /api/v1/uploads/{uploadId}
PUT    /api/v1/uploads/{uploadId}/chunk
POST   /api/v1/uploads/{uploadId}/complete
```

## Task 1: Remove Legacy Runtime and Establish the Single-Crate Skeleton

**Files:**
- Delete: `crates/`
- Delete: legacy SQL files under `migrations/`
- Delete: legacy plans/specs dated `2026-07-31` under `docs/`
- Delete: `docs/origin-tls-phase-5-acceptance.md`
- Replace: `Cargo.toml`
- Replace: `.gitignore`
- Create: `src/lib.rs`
- Create: `src/main.rs`
- Create: `src/{app,auth,config,db,error,files,projects,storage,uploads,web}.rs`
- Create: `tests/common/mod.rs`
- Create: `web/dist/.gitkeep`

- [ ] **Step 1: Record the clean rewrite boundary**

Run:

```powershell
git status --short
git ls-files crates migrations docs web/src
```

Expected: only the approved spec and this plan are new rewrite-era documents; legacy code is still present and tracked.

- [ ] **Step 2: Remove only the legacy implementation named above**

Use `git rm` on the exact tracked legacy paths. Preserve:

```text
docs/superpowers/specs/2026-08-04-cellar-rewrite-mvp-design.md
docs/superpowers/plans/2026-08-04-cellar-rewrite-mvp.md
web/package.json and the Vite/TypeScript toolchain files
```

- [ ] **Step 3: Write the minimal package manifest**

Use one root package named `cellar` with these dependency groups:

```toml
[package]
name = "cellar"
version = "0.1.0"
edition = "2024"
rust-version = "1.93"
publish = false

[dependencies]
axum = { version = "0.8.9", features = ["macros"] }
bytes = "1"
futures-util = "0.3"
http = "1"
http-body-util = "0.1"
jsonwebtoken = "11.0"
mime_guess = "2"
reqwest = { version = "0.13.4", default-features = false, features = ["json", "rustls"] }
rust-embed = "8.12"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sqlx = { version = "0.8", features = ["runtime-tokio", "sqlite", "migrate"] }
thiserror = "2"
time = { version = "0.3", features = ["formatting", "serde"] }
tokio = { version = "1", features = ["full"] }
tokio-util = { version = "0.7", features = ["io"] }
toml = "1.1"
tower-http = { version = "0.7", features = ["request-id", "trace"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
uuid = { version = "1", features = ["serde", "v7"] }

[dev-dependencies]
tempfile = "3"
tower = { version = "0.5", features = ["util"] }
wiremock = "0.6"

[target.'cfg(windows)'.dependencies]
windows-sys = { version = "0.61", features = ["Win32_Foundation", "Win32_Storage_FileSystem"] }
```

- [ ] **Step 4: Add compile-only modules and binary entry point**

`src/lib.rs`:

```rust
pub mod app;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod files;
pub mod projects;
pub mod storage;
pub mod uploads;
pub mod web;
```

Each module initially contains a module-level comment. `main.rs` returns a placeholder `Result` until startup is implemented.

- [ ] **Step 5: Verify the new skeleton**

Run:

```powershell
cargo fmt --check
cargo check
git diff --check
```

Expected: all commands exit 0; no code references a deleted crate.

- [ ] **Step 6: Commit**

```powershell
git add -A
git commit -m "refactor: establish minimal Cellar rewrite"
```

## Task 2: Configuration, Stable Errors, and Request IDs

**Files:**
- Create: `config.example.toml`
- Implement: `src/config.rs`
- Implement: `src/error.rs`
- Test: unit tests inside both modules

- [ ] **Step 1: Write failing configuration tests**

Cover a valid loopback configuration and each fail-closed case:

```rust
#[test]
fn rejects_non_loopback_bind_address() {
    let input = valid_config().replace("127.0.0.1:8787", "0.0.0.0:8787");
    assert_eq!(parse(&input).unwrap_err().code(), "bind_must_be_loopback");
}

#[test]
fn normalizes_owner_email() {
    let config = parse(&valid_config().replace("owner@example.com", " Owner@Example.COM ")).unwrap();
    assert_eq!(config.access.owner_email, "owner@example.com");
}
```

Also reject a non-HTTPS external origin, an origin containing a path/query, empty Access audience, and a relative data root.

- [ ] **Step 2: Confirm the tests fail**

Run `cargo test config::tests -- --nocapture`.

Expected: compilation or assertions fail because parsing and validation are not implemented.

- [ ] **Step 3: Implement the typed TOML contract**

`config.example.toml`:

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

Expose `Config::load(path)`, `Config::parse`, and `Config::validate`. Normalize external origin by accepting only an already canonical `https://host[:port]` string; do not silently repair invalid input.

- [ ] **Step 4: Write failing JSON error tests**

Assert status mapping and serialization for 400, 401, 403, 404, 409, 413, 416, 503, and 507. The `requestId` must be present and details must be optional.

- [ ] **Step 5: Implement `AppError` and request ID propagation**

Define constructors rather than exposing arbitrary status values:

```rust
pub enum AppError {
    BadRequest { code: &'static str, message: String },
    Unauthorized,
    Forbidden,
    NotFound { code: &'static str, message: String },
    Conflict { code: &'static str, message: String, details: Option<Value> },
    PayloadTooLarge,
    RangeNotSatisfiable { size: u64 },
    Unavailable { code: &'static str, message: String },
    InsufficientStorage,
    Internal,
}
```

Store a UUIDv7 request ID in request extensions and copy it to both `X-Request-Id` and JSON errors. Never serialize internal I/O, SQL, or JWT error strings to clients.

- [ ] **Step 6: Verify and commit**

```powershell
cargo test --lib
cargo clippy --all-targets -- -D warnings
git add config.example.toml src/config.rs src/error.rs
git commit -m "feat: add validated runtime configuration"
```

Expected: tests and Clippy exit 0.

## Task 3: SQLite Schema and Repositories

**Files:**
- Create: `migrations/0001_initial.sql`
- Implement: `src/db.rs`
- Test: `src/db.rs` unit tests

- [ ] **Step 1: Write failing repository tests**

Use an isolated temporary on-disk SQLite database. Test:

```rust
#[tokio::test]
async fn project_round_trip_is_creation_ordered() { /* insert two; list oldest first */ }

#[tokio::test]
async fn committed_offset_can_only_advance_from_expected_value() {
    // update 0 -> 32 succeeds; a second update expecting 0 affects zero rows
}
```

Also test upload state transitions `active -> finalizing -> complete`, `active/finalizing -> failed`, and rejection of sizes above `i64::MAX` before SQL.

- [ ] **Step 2: Confirm failure**

Run `cargo test db::tests -- --nocapture`.

- [ ] **Step 3: Add the exact two-table migration**

Use the schema from “API and Persistence Contracts,” add indexes on `upload_session(project_id)` and `upload_session(state)`, enable `PRAGMA foreign_keys = ON`, WAL mode, a 5-second busy timeout, and `synchronous = FULL`.

- [ ] **Step 4: Implement narrow repository methods**

Required methods:

```rust
pub async fn create_project(&self, project: NewProject) -> Result<ProjectRow, DbError>;
pub async fn delete_project(&self, id: Uuid) -> Result<bool, DbError>;
pub async fn list_projects(&self) -> Result<Vec<ProjectRow>, DbError>;
pub async fn get_project(&self, id: Uuid) -> Result<Option<ProjectRow>, DbError>;
pub async fn create_upload(&self, upload: NewUpload) -> Result<UploadRow, DbError>;
pub async fn get_upload(&self, id: Uuid) -> Result<Option<UploadRow>, DbError>;
pub async fn advance_offset(&self, id: Uuid, expected: u64, next: u64) -> Result<bool, DbError>;
pub async fn mark_finalizing(&self, id: Uuid, expected: u64) -> Result<bool, DbError>;
pub async fn mark_complete(&self, id: Uuid) -> Result<(), DbError>;
pub async fn mark_failed(&self, id: Uuid, reason: &str) -> Result<(), DbError>;
pub async fn recoverable_uploads(&self) -> Result<Vec<UploadRow>, DbError>;
```

Keep SQL private and map numeric fields with checked `u64 <-> i64` conversions.

- [ ] **Step 5: Verify and commit**

```powershell
cargo test db::tests
cargo clippy --all-targets -- -D warnings
git add migrations/0001_initial.sql src/db.rs
git commit -m "feat: add project and upload persistence"
```

## Task 4: Windows-Safe Storage Primitives

**Files:**
- Implement: `src/storage.rs`
- Test: `src/storage.rs` unit tests

- [ ] **Step 1: Write the filename validation table test**

The accepted value is one component such as `report 2026.pdf`. Reject:

```rust
let rejected = [
    "", ".", "..", "a/b", "a\\b", "a:b", "trailing.", "trailing ",
    "CON", "con.txt", "PRN", "AUX", "NUL", "COM1", "LPT9", "control\u{1f}.txt",
];
```

Reject values whose UTF-16 representation exceeds 255 code units. Normalize nothing: the validated string is the stored string.

- [ ] **Step 2: Write failing filesystem behavior tests**

Cover:

- project directory creation uses UUID, never the display name;
- staging path is `<root>/.cellar/uploads/<upload-id>.part`;
- final path is `<root>/projects/<project-id>/files/<file-name>`;
- no-replace finalization leaves an existing destination byte-for-byte unchanged;
- truncation changes a staging file to the committed length;
- listing returns only regular files and ignores directories/reparse links.

- [ ] **Step 3: Implement `Storage`**

Expose typed operations, not public path builders:

```rust
pub async fn initialize(&self) -> Result<(), StorageError>;
pub async fn create_project_dir(&self, project_id: Uuid) -> Result<(), StorageError>;
pub async fn remove_empty_project_dir(&self, project_id: Uuid) -> Result<(), StorageError>;
pub async fn create_staging(&self, upload_id: Uuid) -> Result<(), StorageError>;
pub async fn staging_len(&self, upload_id: Uuid) -> Result<Option<u64>, StorageError>;
pub async fn write_chunk(&self, upload_id: Uuid, offset: u64, body: impl AsyncRead) -> Result<u64, StorageError>;
pub async fn truncate_staging(&self, upload_id: Uuid, len: u64) -> Result<(), StorageError>;
pub async fn finalize_no_replace(&self, upload_id: Uuid, project_id: Uuid, name: &SafeFileName) -> Result<(), StorageError>;
pub async fn list_files(&self, project_id: Uuid) -> Result<Vec<DiskFile>, StorageError>;
```

`write_chunk` must seek to the exact offset, stream without buffering the whole chunk, call `sync_data`, and return bytes written. On Windows, finalization uses `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` and with `MOVEFILE_WRITE_THROUGH`; on non-Windows test hosts, use an atomic no-replace link/unlink fallback. All roots and staging/final paths must remain on the configured volume.

- [ ] **Step 4: Verify and commit**

```powershell
cargo test storage::tests
cargo clippy --all-targets -- -D warnings
git add src/storage.rs Cargo.toml Cargo.lock
git commit -m "feat: add safe local storage primitives"
```

## Task 5: Cloudflare Access and Origin Boundary

**Files:**
- Implement: `src/auth.rs`
- Implement: boundary middleware in `src/app.rs`
- Create: `tests/auth_boundary.rs`

- [ ] **Step 1: Write failing Access verification tests**

Use a generated test RSA key and a `wiremock` JWKS endpoint. Assert acceptance only when all of these match:

- algorithm is RS256;
- key ID exists in JWKS;
- `iss` equals configured team domain;
- `aud` contains the configured application audience;
- `exp` is future and `nbf` is not future;
- token type is the Access application token type;
- normalized email equals the configured owner email.

Separately reject missing assertion, bad signature, unknown key, wrong issuer/audience/email, expired token, and future `nbf`.

- [ ] **Step 2: Confirm tests fail**

Run `cargo test --test auth_boundary -- --nocapture`.

- [ ] **Step 3: Implement cached JWKS verification**

Read only `Cf-Access-Jwt-Assertion`. Cache successful JWKS responses for at most one hour, refresh once on unknown `kid`, and fail closed when the key endpoint cannot be reached. Return:

```rust
pub struct OwnerIdentity {
    pub email: String,
}
```

Do not accept identity headers such as `Cf-Access-Authenticated-User-Email` as proof.

- [ ] **Step 4: Add exact-Origin protection tests**

For `POST`, `PUT`, `PATCH`, and `DELETE` under `/api/`, require `Origin` to exactly match `external_origin`. Reject missing, `null`, alternate port, alternate scheme, suffix lookalike, and multiple-value origins with 403. Safe `GET`/`HEAD` requests do not require Origin. Do not add CORS response headers.

- [ ] **Step 5: Build the boundary middleware order**

Order requests as:

```text
request ID -> tracing -> Access authentication -> unsafe-method Origin check -> route
```

The API must never expose a route outside this boundary. Static SPA assets may pass through Access at Cloudflare but need no application JWT parsing.

- [ ] **Step 6: Verify and commit**

```powershell
cargo test --test auth_boundary
cargo clippy --all-targets -- -D warnings
git add src/auth.rs src/app.rs tests/auth_boundary.rs tests/common/mod.rs
git commit -m "feat: enforce Cloudflare Access boundary"
```

## Task 6: Project API with Filesystem/Database Compensation

**Files:**
- Implement: `src/projects.rs`
- Extend: `src/app.rs`
- Create: `tests/projects_api.rs`

- [ ] **Step 1: Write failing project API tests**

Test authenticated requests for:

```json
POST /api/v1/projects
{"name":"Work"}
```

Expected `201`:

```json
{"id":"<uuid>","name":"Work","createdAt":"<rfc3339>"}
```

Also assert empty/whitespace names and names over 100 Unicode scalar values return 400; listing initially returns `[]`; and listing returns only committed database projects in creation order.

- [ ] **Step 2: Add compensation failure tests**

Inject a repository failure after directory creation. Assert:

- the exact empty project directory is removed;
- no project appears in `GET /projects`;
- cleanup failure maps to 503 and is logged with request/project IDs.

- [ ] **Step 3: Implement create/list handlers**

Creation order is:

```text
validate display name
-> allocate UUIDv7
-> create UUID project/files directory with create-new semantics
-> insert SQLite row
-> on insert failure, remove only that exact empty directory
```

Never derive a path from the project display name.

- [ ] **Step 4: Verify and commit**

```powershell
cargo test --test projects_api
cargo clippy --all-targets -- -D warnings
git add src/projects.rs src/app.rs tests/projects_api.rs
git commit -m "feat: add project creation and listing"
```

## Task 7: Upload Session Creation and Status

**Files:**
- Implement session parts of: `src/uploads.rs`
- Extend: `src/app.rs`
- Create: `tests/uploads_api.rs`

- [ ] **Step 1: Write failing create/status tests**

Create request:

```json
POST /api/v1/projects/{projectId}/uploads
{"fileName":"archive.zip","totalSize":"150000000"}
```

Expected `201`:

```json
{
  "id":"<uuid>",
  "projectId":"<uuid>",
  "fileName":"archive.zip",
  "totalSize":"150000000",
  "committedOffset":"0",
  "state":"active"
}
```

Assert status returns the same shape, unknown project/upload returns 404, unsafe filename returns 400, non-decimal or `> i64::MAX` size returns 400, and an existing final filename returns 409.

- [ ] **Step 2: Confirm tests fail**

Run `cargo test --test uploads_api upload_session -- --nocapture` after naming the related tests with the `upload_session_` prefix.

- [ ] **Step 3: Implement decimal string wire types**

Use a reusable serde wrapper:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecimalU64(pub u64);
```

Serialization is always a JSON string; deserialization rejects signs, whitespace, exponent notation, leading `+`, empty strings, and values above `i64::MAX`.

- [ ] **Step 4: Implement session creation atomically enough for the contract**

Validate project and destination, create the staging file with create-new semantics, then insert the session. If the insert fails, remove only the just-created empty staging file. If cleanup also fails, return 503 and log the orphan path without exposing it to the client.

- [ ] **Step 5: Verify and commit**

```powershell
cargo test --test uploads_api upload_session
cargo clippy --all-targets -- -D warnings
git add src/uploads.rs src/app.rs tests/uploads_api.rs
git commit -m "feat: create resumable upload sessions"
```

## Task 8: Sequential Chunk Upload and Idempotent Retry

**Files:**
- Extend: `src/uploads.rs`
- Extend: `src/storage.rs`
- Extend: `tests/uploads_api.rs`

- [ ] **Step 1: Write failing chunk contract tests**

Send `PUT /api/v1/uploads/{id}/chunk` with:

```text
Content-Type: application/octet-stream
Upload-Offset: 0
Content-Length: 33554432
```

Assert:

- exact expected offset appends and returns `204` with `Upload-Offset: 33554432`;
- body length must equal `Content-Length`;
- chunk size above 32 MiB returns 413;
- a chunk that would exceed `totalSize` returns 409;
- an earlier offset returns `204` with the authoritative current offset and does not write;
- a future or overlapping offset returns 409 with `expectedOffset`;
- non-active sessions reject chunks with 409.

- [ ] **Step 2: Write a concurrency test**

Start two requests at offset zero with different bytes. Exactly one may advance the session; the resulting staging file must contain one complete chunk, never an interleaving or double length.

- [ ] **Step 3: Confirm tests fail**

Run `cargo test --test uploads_api chunk -- --nocapture`.

- [ ] **Step 4: Implement per-upload serialization and streaming**

Keep a process-local `HashMap<Uuid, Weak<Mutex<()>>>` lock registry. Under the upload lock:

```text
reload session
-> compare request offset
-> stream at most 32 MiB to the exact staging offset
-> require actual bytes == Content-Length
-> sync_data staging file
-> conditional SQL update expectedOffset -> nextOffset
-> return authoritative offset
```

If streaming ends early or errors, truncate back to the prior committed offset before releasing the lock. If truncation fails, mark the session failed and return 503. Never update SQLite before `sync_data` succeeds.

- [ ] **Step 5: Verify and commit**

```powershell
cargo test --test uploads_api chunk
cargo clippy --all-targets -- -D warnings
git add src/uploads.rs src/storage.rs tests/uploads_api.rs
git commit -m "feat: stream resumable upload chunks"
```

## Task 9: Finalization and Startup Recovery

**Files:**
- Extend: `src/uploads.rs`
- Extend: `src/storage.rs`
- Create: `tests/upload_recovery.rs`

- [ ] **Step 1: Write failing completion tests**

Assert completion:

- rejects when committed offset is not total size;
- transitions `active -> finalizing` before filesystem move;
- flushes staging and moves to destination without replacement;
- verifies destination size;
- marks complete only after verification;
- is idempotent when already complete;
- preserves an existing conflicting destination and marks the session failed.

- [ ] **Step 2: Write the recovery matrix tests**

Create database/filesystem states directly, restart the application fixture, and assert:

| Database state | Staging | Destination | Recovery result |
|---|---:|---:|---|
| active offset N | length N | absent | remains active at N |
| active offset N | length > N | absent | truncate to N, active |
| active offset N | length < N | absent | failed |
| finalizing | absent | matching size | complete |
| finalizing | present | absent | finish no-replace move, complete |
| finalizing | present/absent | conflicting destination | preserve destination, failed |

- [ ] **Step 3: Confirm tests fail**

Run:

```powershell
cargo test --test uploads_api complete -- --nocapture
cargo test --test upload_recovery -- --nocapture
```

- [ ] **Step 4: Implement completion under the same upload lock**

The handler sequence must exactly be:

```text
lock -> reload -> validate full offset -> conditional mark finalizing
-> sync staging -> atomic no-replace move -> verify final size
-> mark complete -> unlock
```

Return 507 for disk-full errors, 409 for a destination conflict, and 503 for ambiguous persistence/storage failures.

- [ ] **Step 5: Run focused recovery at startup**

After migrations and storage initialization, but before binding the listener, load only `active` and `finalizing` sessions and apply the recovery matrix. Log one structured event per repaired or failed session. Do not scan unrelated project files or create a generic operation journal.

- [ ] **Step 6: Verify and commit**

```powershell
cargo test --test uploads_api complete
cargo test --test upload_recovery
cargo clippy --all-targets -- -D warnings
git add src/uploads.rs src/storage.rs tests/upload_recovery.rs tests/uploads_api.rs
git commit -m "feat: finalize and recover uploads safely"
```

## Task 10: Real File Listing and Range Downloads

**Files:**
- Implement: `src/files.rs`
- Extend: `src/app.rs`
- Create: `tests/downloads_api.rs`

- [ ] **Step 1: Write failing listing tests**

Place files directly in the project’s `files` directory and assert `GET /files` returns current disk state:

```json
[
  {"name":"archive.zip","size":"150000000","modifiedAt":"<rfc3339>"}
]
```

Sort case-insensitively by name with original name as deterministic tie-breaker. Ignore directories, staging files, and unsupported special/reparse entries. Missing project returns 404.

- [ ] **Step 2: Write failing download tests**

For a known byte sequence, assert:

- `GET` without Range returns `200`, full bytes, `Accept-Ranges: bytes`, correct `Content-Length` and MIME type;
- `HEAD` returns identical headers and no body;
- `Range: bytes=10-19`, `bytes=10-`, and `bytes=-10` return correct `206` bytes and `Content-Range`;
- an unsatisfiable range returns `416` and `Content-Range: bytes */<size>`;
- malformed or multiple ranges return 416;
- unsafe decoded filename and non-regular file return 404/400 without escaping the project directory.

- [ ] **Step 3: Implement strict single-range parsing**

Use a pure function returning:

```rust
pub struct ByteRange {
    pub start: u64,
    pub end_inclusive: u64,
}
```

Reject commas and arithmetic overflow. For a zero-length file, any Range is unsatisfiable; a normal GET remains `200` with length zero.

- [ ] **Step 4: Stream files**

Open the validated regular file, capture its metadata from the same handle, seek to the selected offset, and stream at most the selected length using `ReaderStream` plus a bounded reader. Do not read the whole file into memory.

- [ ] **Step 5: Verify and commit**

```powershell
cargo test --test downloads_api
cargo clippy --all-targets -- -D warnings
git add src/files.rs src/app.rs tests/downloads_api.rs
git commit -m "feat: list and download project files"
```

## Task 11: Brandless React Shell and Empty-First Project Flow

**Files:**
- Replace: `web/src/main.tsx`
- Replace: `web/src/app.tsx`
- Create: `web/src/types.ts`
- Create: `web/src/api.ts`
- Create: `web/src/styles.css`
- Create: `web/src/components/app-shell.tsx`
- Create: `web/src/components/empty-state.tsx`
- Create: `web/src/components/project-create-dialog.tsx`
- Delete: legacy `web/src/app/`, `web/src/features/`, and legacy style files
- Test: colocated `*.test.tsx`

- [ ] **Step 1: Write failing empty-state and shell tests**

Assert the first screen contains:

- the navigation label `Projects`;
- no Cellar wordmark, icon, or decorative logo;
- no sample project such as `Photos`;
- a single primary `Create project` action;
- only user-returned project names in the sidebar;
- `Uploads` and `Settings` secondary navigation.

- [ ] **Step 2: Confirm tests fail**

Run `npm test` from `web`.

- [ ] **Step 3: Implement the API client and query states**

`api.ts` exports:

```ts
export const api = {
  listProjects(): Promise<Project[]>,
  createProject(name: string): Promise<Project>,
  listFiles(projectId: string): Promise<FileEntry[]>,
};
```

Send `Content-Type: application/json` on JSON mutations and rely on the browser to set the exact same-origin `Origin`. Parse the stable error envelope and surface its user-safe message.

- [ ] **Step 4: Build the selected visual direction**

Use a neutral light-gray canvas, white content surfaces, a narrow left sidebar, restrained blue primary actions, system font stack, 8/12/16/24/32 spacing rhythm, subtle borders, and no gradients or ornamental branding. Desktop uses the sidebar; below 720px it collapses to a top project selector. Respect `prefers-reduced-motion`.

- [ ] **Step 5: Implement project creation interaction**

The dialog focuses the name field, validates 1–100 characters, supports Escape/cancel, disables submit while pending, announces errors, then selects the created project without a full reload.

- [ ] **Step 6: Verify and commit**

```powershell
Set-Location web
npm test
npm run typecheck
npm run lint
npm run build
Set-Location ..
git add web
git commit -m "feat(web): add simple project workspace"
```

## Task 12: File Table and Resumable Browser Upload UI

**Files:**
- Create: `web/src/upload-client.ts`
- Create: `web/src/components/file-table.tsx`
- Create: `web/src/components/upload-panel.tsx`
- Extend: `web/src/app.tsx`
- Extend: `web/src/api.ts`
- Test: `web/src/upload-client.test.ts`, component tests

- [ ] **Step 1: Write failing upload client tests**

Mock `fetch` and a `File` larger than two chunks. Assert the client:

- creates a session with decimal string size;
- queries authoritative status before sending/resuming;
- slices sequential 32 MiB chunks;
- sends exact `Upload-Offset` and browser-generated `Content-Length` equivalent body size;
- adopts the server’s returned offset after an idempotent retry;
- completes only at total size;
- pauses on network failure and can resume with the same selected file.

The browser cannot set `Content-Length` manually. The server must therefore accept the body length provided by the HTTP stack while still enforcing it server-side.

- [ ] **Step 2: Add same-file reselection validation tests**

Persist only session metadata in `localStorage`: upload ID, project ID, name, size, and committed offset. After reload, require the user to choose a local file again. Resume only if name and size match; otherwise show a clear mismatch error. Never claim the browser can reopen a local file automatically.

- [ ] **Step 3: Build the file table**

Show name, formatted size, and modified time. Clicking a filename navigates to the same-origin download endpoint. Include loading, error, and genuinely empty states; do not generate preview covers or synthetic data.

- [ ] **Step 4: Build the upload panel**

Support file picker and drag/drop, one active upload at a time, determinate progress, byte counts, pause caused by connectivity, retry, reload recovery prompt, and completion refresh of the real file list. Keep a compact upload status entry reachable from `Uploads`.

- [ ] **Step 5: Verify and commit**

```powershell
Set-Location web
npm test
npm run typecheck
npm run lint
npm run build
Set-Location ..
git add web
git commit -m "feat(web): add resumable uploads and file list"
```

## Task 13: Embedded Web App, Startup, and Graceful Shutdown

**Files:**
- Implement: `src/web.rs`
- Implement: `src/main.rs`
- Complete: `src/app.rs`
- Create: `scripts/run-dev.ps1`
- Extend: `.gitignore`
- Test: integration tests in `src/web.rs` and `tests/common/mod.rs`

- [ ] **Step 1: Write failing static serving tests**

Assert:

- `/` returns embedded `index.html`;
- hashed assets return correct content types and long immutable cache headers;
- unknown non-API paths fall back to `index.html`;
- unknown `/api/*` paths return JSON 404, never HTML;
- CSP, `X-Content-Type-Options: nosniff`, and `Referrer-Policy: no-referrer` are present.

- [ ] **Step 2: Implement embedded assets**

Use `rust-embed` over `web/dist/`. Keep `web/dist/.gitkeep` as the compile-time empty-directory placeholder and document that release builds must run `npm run build` before `cargo build --release`.

- [ ] **Step 3: Implement startup in strict order**

`main.rs`:

```text
load CELLAR_CONFIG path
-> validate configuration
-> initialize structured logging
-> initialize storage directories
-> connect/migrate SQLite
-> recover upload sessions
-> construct Access verifier and router
-> bind configured loopback address
-> serve until Ctrl+C/service shutdown
-> stop accepting and drain requests
```

Refuse startup if the bind address is not loopback, configuration is invalid, migration fails, or recovery returns an ambiguous error.

- [ ] **Step 4: Add a development launcher**

`scripts/run-dev.ps1` checks for `web/node_modules`, builds the frontend once, sets `CELLAR_CONFIG` to a caller-supplied path, and runs the Rust server. It must not provision Cloudflare, alter Windows services, or write outside configured data paths.

- [ ] **Step 5: Verify and commit**

```powershell
Set-Location web
npm run build
Set-Location ..
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
git add src scripts .gitignore web/dist/.gitkeep
git commit -m "feat: serve Cellar as one local application"
```

## Task 14: Operator Runbook and End-to-End Acceptance

**Files:**
- Replace: `README.md`
- Create: `docs/cloudflare-access-setup.md`
- Create: `scripts/check.ps1`
- Create: `tests/acceptance_local.rs`

- [ ] **Step 1: Add a local acceptance test**

Start the router with fake verified identity and temporary storage, then execute one full flow:

```text
create project
-> create 96 MiB upload
-> send three 32 MiB chunks
-> recreate app state to simulate restart
-> query authoritative offset
-> complete
-> list actual file
-> full download byte comparison
-> range download byte comparison
```

Also run a separate interrupted upload where the staging file has trailing bytes and prove startup truncates to the committed offset.

- [ ] **Step 2: Write the operator documentation**

README must cover prerequisites, config creation, frontend/release build, local start, data layout, backup boundaries, logs, and upgrade order. `docs/cloudflare-access-setup.md` must cover:

1. create a named Cloudflare Tunnel pointing only to `http://127.0.0.1:8787`;
2. attach the chosen public hostname;
3. create a self-hosted Access application;
4. allow only the owner identity;
5. copy the application AUD, team domain, exact HTTPS origin, and normalized owner email into config;
6. run `cloudflared` separately;
7. verify unauthenticated redirect, authenticated UI, and direct API rejection without a valid assertion.

State explicitly that Quick Tunnels are not the protected production setup and that Cellar never binds a LAN/public interface.

- [ ] **Step 3: Add the repeatable check script**

`scripts/check.ps1` runs, in order:

```powershell
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
Push-Location web
npm test
npm run typecheck
npm run lint
npm run build
Pop-Location
cargo build --release
```

The script stops on the first failure and returns a non-zero exit code.

- [ ] **Step 4: Run automated acceptance**

```powershell
.\scripts\check.ps1
git diff --check
git status --short
```

Expected: every command exits 0; only intentional documentation or final fixture changes remain.

- [ ] **Step 5: Run manual browser acceptance**

With a real config and named Cloudflare tunnel:

- open the public hostname from desktop and mobile widths;
- authenticate through Access;
- confirm the initial state has no sample projects;
- create a project;
- upload a file larger than 100 MB;
- stop/restart the Rust process between chunks, reselect the same local file, and resume;
- compare downloaded bytes with the source;
- request a range and compare the selected bytes;
- attempt a same-name upload and confirm the original remains unchanged.

If real Cloudflare credentials/domain are unavailable, mark only this manual subsection blocked; do not weaken or bypass authentication to make it pass.

- [ ] **Step 6: Commit documentation and acceptance coverage**

```powershell
git add README.md docs/cloudflare-access-setup.md scripts/check.ps1 tests/acceptance_local.rs
git commit -m "docs: add setup and acceptance runbook"
```

## Task 15: Final Spec Audit

**Files:**
- Review: all rewrite files
- Compare: `docs/superpowers/specs/2026-08-04-cellar-rewrite-mvp-design.md`

- [ ] **Step 1: Scan for accidental legacy scope and placeholders**

Run:

```powershell
rg -n "TODO|TBD|FIXME|Photos|logo|tag|preview|trash|rename|move|copy|public link|microservice" src web/src README.md docs/cloudflare-access-setup.md
```

Expected: no placeholder or sample-data hits; any documentation mention of excluded features is intentional and reviewed.

- [ ] **Step 2: Audit every acceptance requirement**

Create a temporary checklist from the approved design and point each item to an automated test or the named manual Cloudflare check. Specifically verify the 32 MiB chunk limit, decimal strings, owner email match, exact Origin, startup recovery matrix, no-replace completion, real directory listing, HEAD, and single-range behavior.

- [ ] **Step 3: Run the complete verification from a clean build state**

Do not delete user data. Remove only repository build outputs (`target/` and `web/dist/`), rebuild, then run:

```powershell
npm ci --prefix web
npm run build --prefix web
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
npm test --prefix web
npm run typecheck --prefix web
npm run lint --prefix web
cargo build --release
git diff --check
```

Expected: all exit 0.

- [ ] **Step 4: Inspect final repository state**

```powershell
git status --short
git log --oneline --decorate -15
```

Expected: working tree clean; commits are small and correspond to the tasks above.

- [ ] **Step 5: Request code review before integration**

Use the `requesting-code-review` skill against the full diff from `develop` to `codex/rewrite-mvp`. Resolve correctness/security findings, rerun the complete verification, and only then offer merge/PR choices via `finishing-a-development-branch`.

## Implementation Guardrails

- Never expose a bypass flag that disables Access validation in a production router. Tests inject a fake verifier directly into app construction.
- Never bind beyond loopback, even when `cloudflared` is unavailable.
- Never use a project display name or unvalidated filename to construct a path.
- Never advance `committed_offset` before the staging file is flushed.
- Never overwrite an existing destination during finalization.
- Never treat SQLite as the file listing source.
- Never buffer a full large upload or download in memory.
- Never add folders, tags, search, previews, trash, rename/move/copy, public links, installers, automatic tunnel management, or microservices to this MVP.
- Preserve `develop` throughout implementation; all rewrite commits stay on `codex/rewrite-mvp` until explicit integration approval.

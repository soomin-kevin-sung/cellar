# Simple File Transfer MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace resumable upload sessions with one-request, automatically cleaned uploads while retaining projects, file listing, and downloads.

**Architecture:** `POST /api/v1/projects/{project_id}/uploads?fileName=...` streams one octet-stream request into a UUID-named temporary file, atomically publishes it without overwrite, and removes temporary data on every failure. SQLite remains responsible for projects only; completed files are listed from the project filesystem. The browser performs one `fetch`, presents success or failure, and retries failed work from the beginning.

**Tech Stack:** Rust 1.93, Axum 0.8, Tokio, SQLite/sqlx, React 19, TypeScript, Vitest

---

## File Map

- `src/uploads.rs`: small one-request upload service, route, validation, response, and error mapping.
- `src/storage.rs`: exact temporary-file creation/write/finalization plus safe startup cleanup.
- `src/db.rs`, `migrations/0002_remove_upload_sessions.sql`: remove upload-session persistence; retain project persistence without changing an applied migration.
- `src/main.rs`: run temporary-file cleanup instead of session recovery.
- `src/json_size.rs`, `src/lib.rs`, `src/files.rs`: share the decimal JSON size serializer used by upload and list responses.
- `tests/uploads_api.rs`: upload success and failure integration coverage.
- `tests/upload_recovery.rs`: remove obsolete resumable-session recovery suite.
- `web/src/upload-client.ts`: one-request upload client without local storage or session recovery.
- `web/src/components/upload-panel.tsx`: idle/uploading/complete/error UI only.
- `web/src/types.ts`, `web/src/api.ts`: remove session types and session API calls.
- `web/src/**/*.test.*`: replace recovery expectations with one-request success/failure expectations.
- `README.md`, `docs/cloudflare-access-setup.md`: describe the reduced MVP and restart-from-zero behavior.

### Task 1: Single-request server upload

**Files:**
- Replace: `tests/uploads_api.rs`
- Replace: `src/uploads.rs`
- Modify: `src/app.rs`

- [ ] **Step 1: Write failing API tests**

Cover a raw request shaped as:

```rust
Request::builder()
    .method("POST")
    .uri(format!("/api/v1/projects/{project_id}/uploads?fileName=report.bin"))
    .header("origin", EXTERNAL_ORIGIN)
    .header(ASSERTION_HEADER, VALID_ASSERTION)
    .header("content-type", "application/octet-stream")
    .body(Body::from(b"cellar-data".as_slice()))
    .unwrap()
```

Assert `201 Created`, response `{"name":"report.bin","size":"11"}`, exact stored bytes, and appearance in `GET /files`. Add focused tests for missing project, invalid/canonical UUID, invalid filename, wrong content type, duplicate destination, empty file, and an erroring body stream that leaves no `.part` file.

- [ ] **Step 2: Run the focused tests and confirm RED**

Run: `cargo test --test uploads_api`

Expected: failures because the current endpoint expects JSON session creation and separate chunks.

- [ ] **Step 3: Implement the minimal upload service**

Replace session operations with these boundaries:

```rust
pub trait UploadRepository: Send + Sync {
    fn get_project<'a>(&'a self, project_id: Uuid)
        -> UploadFuture<'a, Result<Option<ProjectRow>, DbError>>;
}

pub trait UploadStorage: Send + Sync {
    fn create_staging<'a>(&'a self, upload_id: Uuid) -> StorageFuture<'a, ()>;
    fn write_upload<'a>(&'a self, upload_id: Uuid, reader: UploadBodyReader)
        -> StorageFuture<'a, u64>;
    fn sync_staging<'a>(&'a self, upload_id: Uuid) -> StorageFuture<'a, ()>;
    fn finalize_no_replace<'a>(&'a self, upload_id: Uuid, project_id: Uuid,
        name: &'a SafeFileName) -> StorageFuture<'a, ()>;
    fn remove_staging<'a>(&'a self, upload_id: Uuid) -> StorageFuture<'a, ()>;
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UploadQuery { file_name: String }

#[derive(Serialize)]
struct UploadResponse { name: String, size: DecimalU64 }
```

The handler must accept exactly `application/octet-stream`, convert `Body::into_data_stream()` through `StreamReader`, and call `UploadService::upload`. The service verifies the project, validates `SafeFileName`, creates one UUID staging file, streams from offset zero, syncs, then calls `finalize_no_replace`. On every error it awaits `remove_staging`; a small armed drop guard schedules the same exact cleanup if request cancellation drops the future. Return `201` only after final publication.

- [ ] **Step 4: Run focused server tests and confirm GREEN**

Run: `cargo test --test uploads_api`

Expected: all single-request upload tests pass.

- [ ] **Step 5: Commit the server slice**

```powershell
git add src/uploads.rs src/app.rs tests/uploads_api.rs
git commit -m "feat: replace upload sessions with one-request uploads"
```

### Task 2: Automatic temporary-file cleanup and persistence removal

**Files:**
- Modify: `src/storage.rs`
- Modify: `src/main.rs`
- Modify: `src/db.rs`
- Modify: `src/files.rs`
- Create: `src/json_size.rs`
- Modify: `src/lib.rs`
- Create: `migrations/0002_remove_upload_sessions.sql`
- Delete: `tests/upload_recovery.rs`
- Test: `src/storage.rs`
- Test: `src/main.rs`

- [ ] **Step 1: Write failing cleanup tests**

Add storage tests that create canonical `<uuid>.part` regular files, run `cleanup_staging()`, and assert they are absent. Assert unrelated names are preserved and a canonical `.part` directory or reparse/symlink is preserved and returns `StorageError::UnsafeManagedEntry`. Change the startup-order test expectation from `"recovery"` to `"temporary_cleanup"`.

- [ ] **Step 2: Run cleanup tests and confirm RED**

Run:

```powershell
cargo test storage::tests::cleanup_staging
cargo test --bin cellar startup_runs_project_audit_before_temporary_cleanup_and_bind
```

Expected: failures because `cleanup_staging` and the renamed startup step do not exist.

- [ ] **Step 3: Implement exact startup cleanup**

Add:

```rust
pub async fn cleanup_staging(&self) -> Result<(), StorageError> {
    let _guard = self.mutation_lock.lock().await;
    self.require_uploads_dir().await?;
    let mut entries = fs::read_dir(self.uploads_dir()).await.map_err(map_io)?;
    while let Some(entry) = entries.next_entry().await.map_err(map_io)? {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
        let Some(id) = name.strip_suffix(".part").and_then(|s| Uuid::parse_str(s).ok()) else { continue };
        if format!("{id}.part") != name { continue; }
        let metadata = fs::symlink_metadata(entry.path()).await.map_err(map_io)?;
        require_safe_regular_file_metadata(&metadata)
            .map_err(|_| StorageError::UnsafeManagedEntry)?;
        fs::remove_file(entry.path()).await.map_err(map_io)?;
    }
    Ok(())
}
```

Rename the startup trait step to `cleanup_temporary_uploads` and invoke `Storage::cleanup_staging`. Remove upload-session rows, transitions, recovery types, and their tests from `db.rs`; add `migrations/0002_remove_upload_sessions.sql` containing `DROP TABLE upload_session;`. Move `DecimalU64` into `src/json_size.rs`, export it from `src/lib.rs`, and import it from both `uploads.rs` and `files.rs` so uploads no longer own a general response type.

- [ ] **Step 4: Run backend tests and confirm GREEN**

Run: `cargo test --all-targets --all-features`

Expected: all backend tests pass with no session-recovery suite.

- [ ] **Step 5: Commit cleanup and persistence simplification**

```powershell
git add src/storage.rs src/main.rs src/db.rs src/files.rs src/json_size.rs src/lib.rs migrations/0002_remove_upload_sessions.sql tests/upload_recovery.rs
git commit -m "refactor: remove resumable upload persistence"
```

### Task 3: One-request browser experience

**Files:**
- Replace: `web/src/upload-client.ts`
- Replace: `web/src/upload-client.test.ts`
- Modify: `web/src/components/upload-panel.tsx`
- Modify: `web/src/components/upload-panel.test.tsx`
- Modify: `web/src/types.ts`
- Modify: `web/src/api.ts`
- Modify: `web/src/api.test.ts`
- Modify: `web/src/app.test.tsx`

- [ ] **Step 1: Write failing client and component tests**

Define the only client contract as:

```ts
export interface UploadResult { name: string; size: string }
export interface UploadOptions { projectId: string; file: File; signal?: AbortSignal }
export async function uploadFile(options: UploadOptions): Promise<UploadResult>
```

Test one `fetch` call to `/api/v1/projects/<encoded>/uploads?fileName=<encoded>` with method `POST`, `Content-Type: application/octet-stream`, the `File` as body, and the signal. Test safe server-envelope errors and network failure. Component tests must assert idle -> uploading -> complete, failure -> error with a single `Retry upload` action, retry starts the full file again, successful completion refreshes the list, and no paused/recovered/cancel UI or local-storage access exists.

- [ ] **Step 2: Run web tests and confirm RED**

Run: `npm test -- --run` from `web`

Expected: failures because the existing client creates, chunks, stores, and resumes sessions.

- [ ] **Step 3: Implement the minimal browser client and panel**

Use one request:

```ts
const response = await fetch(
  `/api/v1/projects/${encodeURIComponent(projectId)}/uploads?fileName=${encodeURIComponent(file.name)}`,
  { method: "POST", headers: { "Content-Type": "application/octet-stream" }, body: file, signal },
);
```

Validate the successful `{name,size}` response and reuse the safe API error-envelope message behavior. Reduce panel state to `"idle" | "uploading" | "complete" | "error"`; retain the selected `File` only so `Retry upload` resends it from byte zero. Remove `UploadSession`, storage keys, resume matching, paused/retryable classes, recovery cards, and corresponding API methods.

- [ ] **Step 4: Run web verification and confirm GREEN**

Run from `web`:

```powershell
npm test -- --run
npm run typecheck
npm run lint
npm run build
```

Expected: all commands exit zero.

- [ ] **Step 5: Commit the browser slice**

```powershell
git add web/src
git commit -m "feat(web): use one-request file uploads"
```

### Task 4: Documentation and end-to-end verification

**Files:**
- Modify: `README.md`
- Modify: `docs/cloudflare-access-setup.md`
- Modify: `tests/acceptance_local.rs`

- [ ] **Step 1: Update acceptance coverage and docs**

Change the local acceptance upload from create/chunk/complete calls to one POST with the file body, then assert list and downloaded bytes. Document that interruption fails the upload, temporary data is automatically removed, retry starts from zero, and Windows filesystem watching is deferred.

- [ ] **Step 2: Run the complete verification suite**

Run:

```powershell
cargo fmt --all -- --check
cargo test --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
Push-Location web
npm test -- --run
npm run typecheck
npm run lint
npm run build
Pop-Location
cargo build --release --all-features
```

Expected: every command exits zero and generated `web/dist` artifacts are removed except `web/dist/.gitkeep` before committing.

- [ ] **Step 3: Review the final diff against the spec**

Confirm no public status/chunk/complete endpoint, local-storage upload metadata, recovery UI, failed-session persistence, or file-watch implementation remains. Confirm project creation/listing, upload/list/download, Cloudflare boundary, duplicate rejection, and temporary cleanup remain.

- [ ] **Step 4: Commit the verified MVP**

```powershell
git add README.md docs/cloudflare-access-setup.md tests/acceptance_local.rs web/dist/.gitkeep
git commit -m "docs: finalize simple file transfer MVP"
```

# Cellar Phase 2 Core Files Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver crash-safe project and file management, resumable uploads, ranged downloads, no-overwrite mutations, and recoverable trash on the Phase 1 service foundation.

**Architecture:** Keep filesystem operations behind handle-based storage ports and serialize mutations per project. Every mutation creates a versioned operation journal entry, publishes only through atomic no-replace operations, and exposes stable versioned HTTP contracts.

**Tech Stack:** Rust, Axum, Tokio, SQLx SQLite, Windows handle APIs, SHA-256, tower-http.

---

### Task 1: Implement project domain, repository, and API

**Files:**
- Create: `crates/cellar-core/src/project.rs`
- Create: `crates/cellar-db/src/project_repo.rs`
- Create: `crates/cellar-api/src/routes/projects.rs`
- Test: `crates/cellar-api/tests/projects.rs`

- [ ] **Step 1: Write failing project lifecycle tests**

```rust
#[tokio::test]
async fn project_lifecycle_uses_optimistic_versioning() {
    let app = owner_app().await;
    let p = app.create_project("Cellar", "Files").await.assert_created();
    app.patch_project(p.id, p.version, "Cellar v2").await.assert_ok();
    app.patch_project(p.id, p.version, "stale").await.assert_status(409);
    app.archive_project(p.id).await.assert_ok();
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p cellar-api --test projects`

Expected: FAIL because project ports and routes do not exist.

- [ ] **Step 3: Implement project types and repository**

```rust
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub description: String,
    pub status: ProjectStatus,
    pub version: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

pub trait ProjectRepository: Send + Sync {
    async fn create(&self, draft: NewProject) -> Result<Project, CellarError>;
    async fn update(&self, id: ProjectId, expected: i64, patch: ProjectPatch)
        -> Result<Project, CellarError>;
}
```

Create `projects/<uuid>/files` through the operation journal and no-replace directory create before returning `201`.

- [ ] **Step 4: Verify API contract**

Run: `cargo test -p cellar-api --test projects`

Expected: PASS for create, list, edit, archive, invalid name, stale version, and duplicate request idempotency.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-core crates/cellar-db crates/cellar-api
git commit -m "feat: add project lifecycle"
```

### Task 2: Implement safe names, handle-relative storage, and no-replace operations

**Files:**
- Create: `crates/cellar-storage/src/names.rs`
- Create: `crates/cellar-storage/src/traits.rs`
- Create: `crates/cellar-windows/src/handles.rs`
- Create: `crates/cellar-windows/src/names.rs`
- Test: `crates/cellar-windows/tests/path_security.rs`

- [ ] **Step 1: Write failing path-security tests**

```rust
#[test_case(".."; "parent")]
#[test_case(r"\\server\share"; "unc")]
#[test_case("file.txt:secret"; "ads")]
#[test_case("CON"; "reserved")]
fn rejects_dangerous_names(name: &str) {
    assert!(WindowsName::parse(name).is_err());
}

#[test]
fn no_replace_create_preserves_existing_destination() {
    let fs = ntfs_fixture();
    fs.write("present.txt", b"owner");
    assert_eq!(fs.create_no_replace("present.txt").unwrap_err().code(), "conflict");
    assert_eq!(fs.read("present.txt"), b"owner");
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p cellar-windows --test path_security`

Expected: FAIL.

- [ ] **Step 3: Define the storage port and Windows adapter**

```rust
pub trait Storage: Send + Sync {
    async fn open_verified(&self, parent: EntryHandle, name: &SafeName)
        -> Result<VerifiedHandle, StorageError>;
    async fn rename_no_replace(
        &self,
        source: &VerifiedHandle,
        destination_parent: &VerifiedHandle,
        destination_name: &SafeName,
    ) -> Result<FileIdentity, StorageError>;
}
```

Open each component relative to trusted handles, use `FILE_FLAG_OPEN_REPARSE_POINT`, reject link count greater than one, verify final volume/path, and perform mutation on the verified handle.

- [ ] **Step 4: Run storage security tests**

Run: `cargo test -p cellar-windows --test path_security`

Expected: PASS for traversal, UNC, ADS, reserved names, dots/spaces, reparse points, hard links, case-sensitive directories, race replacement, and no-overwrite.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-storage crates/cellar-windows
git commit -m "feat: add secure Windows storage adapter"
```

### Task 3: Add file catalog and cursor pagination

**Files:**
- Create: `crates/cellar-core/src/file_entry.rs`
- Create: `crates/cellar-db/src/file_repo.rs`
- Create: `crates/cellar-api/src/routes/files.rs`
- Test: `crates/cellar-api/tests/file_listing.rs`

- [ ] **Step 1: Write failing listing tests**

```rust
#[tokio::test]
async fn lists_stably_with_cursor_and_windows_ordering() {
    let app = project_with_files(["a.txt", "A2.txt", "b.txt"]).await;
    let first = app.list_files(None, 2).await.assert_ok();
    let second = app.list_files(first.next_cursor, 2).await.assert_ok();
    assert_no_duplicates(first.items, second.items);
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p cellar-api --test file_listing`

Expected: FAIL.

- [ ] **Step 3: Implement catalog query**

```rust
pub struct FilePage {
    pub items: Vec<FileEntry>,
    pub next_cursor: Option<String>,
    pub snapshot_version: i64,
}

pub struct FileCursor {
    pub windows_name_key: Vec<u8>,
    pub entry_id: FileEntryId,
}
```

Default to 100 items, cap at 500, and order by Windows comparison key then logical ID. Return project-relative paths only.

- [ ] **Step 4: Verify listing**

Run: `cargo test -p cellar-api --test file_listing`

Expected: PASS for root/child folders, page boundaries, stable snapshots, invalid cursors, and max limit.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-core crates/cellar-db crates/cellar-api
git commit -m "feat: add paginated file catalog"
```

### Task 4: Implement upload session creation, status, chunk, and cancel

**Files:**
- Create: `crates/cellar-core/src/upload.rs`
- Create: `crates/cellar-db/src/upload_repo.rs`
- Create: `crates/cellar-api/src/routes/uploads.rs`
- Test: `crates/cellar-api/tests/resumable_upload.rs`

- [ ] **Step 1: Write failing protocol tests**

```rust
#[tokio::test]
async fn resumes_only_from_committed_offset() {
    let s = create_upload("67108864").await;
    put_chunk(&s, "0", bytes_32_mib()).await.assert_no_content();
    assert_eq!(status(&s).await.committed_offset, "33554432");
    put_chunk(&s, "67108864", b"gap").await.assert_status(409);
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p cellar-api --test resumable_upload`

Expected: FAIL.

- [ ] **Step 3: Implement session and chunk state machine**

```rust
pub struct UploadStatus {
    pub id: UploadId,
    pub expected_size: i64,
    pub committed_offset: i64,
    pub max_chunk_size: i64,
    pub expires_at: OffsetDateTime,
}
```

Parse JSON sizes as decimal strings. Before write, persist pending offset/length/digest; write exact range; call `FlushFileBuffers`; compare-and-swap committed offset; clear pending. Enforce 32 MiB body, digest, session count, concurrency, free-space ledger, and expiry.

- [ ] **Step 4: Run upload tests**

Run: `cargo test -p cellar-api --test resumable_upload`

Expected: PASS for zero-byte, short final chunk, duplicate retry, mismatch, overlap, gap, crash-tail truncation, short durable data failure, cancellation, expiration, `413`, `429`, and `507`.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-core crates/cellar-db crates/cellar-api
git commit -m "feat: add resumable upload sessions"
```

### Task 5: Finalize uploads with journaled atomic publication

**Files:**
- Create: `crates/cellar-core/src/operation.rs`
- Create: `crates/cellar-db/src/operation_repo.rs`
- Create: `crates/cellar-service/src/recovery.rs`
- Test: `crates/cellar-api/tests/upload_finalize.rs`
- Test: `crates/cellar-e2e/tests/upload_finalize.rs`

- [ ] **Step 1: Write failing finalize fault tests**

```rust
#[test_case(FaultPoint::AfterIntent)]
#[test_case(FaultPoint::AfterRename)]
#[test_case(FaultPoint::AfterCatalogCommit)]
fn restart_never_exposes_partial_or_duplicates(point: FaultPoint) {
    let result = crash_and_restart_upload(point);
    result.assert_exactly_one_complete_or_one_resumable_staging();
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p cellar-api --test upload_finalize`

Expected: FAIL.

- [ ] **Step 3: Implement finalization order**

```rust
pub async fn finalize_upload(ctx: &AppContext, id: UploadId)
    -> Result<FileEntry, CellarError>
{
    let verified = ctx.uploads.verify_size_and_sha256(id).await?;
    ctx.operations.record_commit_intent(&verified).await?;
    let identity = ctx.storage.publish_no_replace(&verified).await?;
    ctx.db.complete_upload_and_catalog(id, identity).await
}
```

Close staging before rename, verify result identity after rename, and make catalog plus operation completion one DB transaction. Recovery uses the design's source/destination decision table.

- [ ] **Step 4: Run finalize and fault tests**

Run:

```powershell
cargo test -p cellar-api --test upload_finalize
cargo test -p cellar-e2e --test upload_finalize
```

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-core crates/cellar-db crates/cellar-service crates/cellar-api crates/cellar-e2e
git commit -m "feat: finalize uploads atomically"
```

### Task 6: Implement single-range downloads

**Files:**
- Create: `crates/cellar-storage/src/ranges.rs`
- Modify: `crates/cellar-api/src/routes/files.rs`
- Test: `crates/cellar-api/tests/ranged_download.rs`

- [ ] **Step 1: Write failing HTTP range matrix tests**

```rust
#[test_case("bytes=0-9", 206, Some("bytes 0-9/100"))]
#[test_case("bytes=-10", 206, Some("bytes 90-99/100"))]
#[test_case("bytes=100-", 416, Some("bytes */100"))]
#[test_case("garbage", 200, None)]
fn range_contract(header: &str, status: u16, content_range: Option<&str>) {
    assert_response(header, status, content_range);
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p cellar-api --test ranged_download`

Expected: FAIL.

- [ ] **Step 3: Implement parser and stable-handle streaming**

```rust
pub enum RangeDecision {
    Full,
    Partial { start: u64, end_inclusive: u64 },
    Unsatisfiable { len: u64 },
}
```

Ignore malformed and multiple ranges with `200`; return exact `206/416`; make HEAD header-equivalent; return full `200` on If-Range mismatch. Use SHA-256 ETag only when ready and stream through the handle used for metadata verification.

- [ ] **Step 4: Run range tests**

Run: `cargo test -p cellar-api --test ranged_download`

Expected: PASS for empty files, suffix/open-ended, HEAD, multi-range, If-Range match/mismatch, identity change, and content disposition injection.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-storage crates/cellar-api
git commit -m "feat: add ranged file downloads"
```

### Task 7: Add rename, move, copy, and case-only recovery

**Files:**
- Modify: `crates/cellar-api/src/routes/files.rs`
- Modify: `crates/cellar-service/src/recovery.rs`
- Test: `crates/cellar-api/tests/file_mutations.rs`
- Test: `crates/cellar-e2e/tests/file_mutations.rs`

- [ ] **Step 1: Write failing conflict and fault tests**

```rust
#[tokio::test]
async fn explorer_destination_wins_without_overwrite() {
    let op = begin_move("a.txt", "b.txt").await;
    explorer_create("b.txt", b"external");
    op.commit().await.assert_status(409);
    assert_eq!(read("b.txt"), b"external");
    assert_eq!(read("a.txt"), b"original");
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p cellar-api --test file_mutations`

Expected: FAIL.

- [ ] **Step 3: Implement per-project serialization and recovery payloads**

```rust
pub struct NamespaceIntent {
    pub source: ExpectedIdentity,
    pub destination_parent: ExpectedIdentity,
    pub destination_name: SafeName,
    pub temporary_name: Option<SafeName>,
    pub payload_version: u32,
}
```

Use no-replace operations. Copy uses hidden staging identity. Case-only rename uses source → generated temporary → final. Recovery implements every table in design sections 13.2–13.3.

- [ ] **Step 4: Run mutation and crash tests**

Run:

```powershell
cargo test -p cellar-api --test file_mutations
cargo test -p cellar-e2e --test file_mutations
```

Expected: PASS at every durable phase.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-api crates/cellar-service crates/cellar-e2e
git commit -m "feat: add crash-safe file mutations"
```

### Task 8: Add trash, restore, and project deletion

**Files:**
- Create: `crates/cellar-api/src/routes/trash.rs`
- Modify: `crates/cellar-db/src/operation_repo.rs`
- Modify: `crates/cellar-service/src/recovery.rs`
- Test: `crates/cellar-api/tests/trash.rs`
- Test: `crates/cellar-e2e/tests/project_delete.rs`

- [ ] **Step 1: Write failing trash tests**

```rust
#[tokio::test]
async fn restore_never_overwrites() {
    let item = trash("report.pdf").await;
    create("report.pdf", b"new");
    restore(item.id).await.assert_status(409);
    assert_eq!(read("report.pdf"), b"new");
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p cellar-api --test trash`

Expected: FAIL.

- [ ] **Step 3: Implement unique trash payloads and leases**

```rust
pub fn trash_payload(root: &Path, project: ProjectId, item: TrashId) -> PathBuf {
    root.join(".cellar")
        .join("trash")
        .join(project.to_string())
        .join(item.to_string())
        .join("payload")
}
```

Cancel uploads, drain handles, move no-replace, verify identity, then commit deleted state. Restore metadata and cover only after the UUID destination is absent and the reverse move succeeds. Purge honors leases and retention.

- [ ] **Step 4: Run Phase 2 verification**

Run:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-api crates/cellar-db crates/cellar-service crates/cellar-e2e
git commit -m "feat: add recoverable trash lifecycle"
```

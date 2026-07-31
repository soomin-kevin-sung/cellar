# Cellar Phase 3 Explorer Reconciliation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reconcile direct Windows Explorer changes with Cellar's catalog while preserving safe NTFS identity, bounded recovery, hashing, and backup behavior.

**Architecture:** Treat watcher events as hints, serialize catalog writes per project, fence scans with dirty epochs, and use NTFS identity only when unique and supported. Keep the filesystem authoritative while operation and scan metadata make recovery deterministic.

**Tech Stack:** Rust, Tokio bounded queues, Windows `ReadDirectoryChangesW` and file identity APIs, SQLx SQLite, SHA-256.

---

### Task 1: Implement Windows identity and ordinal filename comparison

**Files:**
- Create: `crates/cellar-windows/src/identity.rs`
- Modify: `crates/cellar-windows/src/names.rs`
- Modify: `crates/cellar-db/src/pool.rs`
- Test: `crates/cellar-windows/tests/ntfs_identity.rs`
- Test: `crates/cellar-db/tests/windows_collation.rs`

- [ ] **Step 1: Write failing identity tests**

```rust
#[test]
fn rename_preserves_unique_ntfs_identity() {
    let f = fixture_file("before.txt");
    let before = identity(&f);
    rename("before.txt", "after.txt");
    assert_eq!(before, identity("after.txt"));
}

#[test]
fn windows_collation_treats_ascii_case_as_equal_without_normalizing_unicode() {
    assert_eq!(windows_compare("Report.txt", "report.TXT"), Ordering::Equal);
    assert_ne!(exact_bytes("é.txt"), exact_bytes("e\u{301}.txt"));
}
```

- [ ] **Step 2: Verify failure**

Run:

```powershell
cargo test -p cellar-windows --test ntfs_identity
cargo test -p cellar-db --test windows_collation
```

Expected: FAIL.

- [ ] **Step 3: Implement opaque platform identity**

```rust
pub struct WindowsFileIdentity {
    pub volume_serial: [u8; 8],
    pub file_id: [u8; 16],
}

pub enum IdentityMatch {
    Unique(FileEntryId),
    ReplacedAtSamePath(FileEntryId),
    Ambiguous,
    Unsupported,
}
```

Read `FILE_ID_INFO`, reject zero IDs and link count greater than one, register `WINDOWS_ORDINAL_CI` using `CompareStringOrdinal`, and version the collation.

- [ ] **Step 4: Run identity tests**

Run: `cargo test -p cellar-windows --test ntfs_identity; cargo test -p cellar-db --test windows_collation`

Expected: PASS for rename, move, atomic replacement revision, hard link unsupported, case-sensitive subtree unsupported, and exact-name preservation.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-windows crates/cellar-db
git commit -m "feat: add NTFS identity and collation"
```

### Task 2: Add the bounded Windows watcher

**Files:**
- Create: `crates/cellar-windows/src/watcher.rs`
- Test: `crates/cellar-windows/tests/watcher.rs`
- Test: `crates/cellar-windows/tests/watcher_overflow.rs`

- [ ] **Step 1: Write failing watcher tests**

```rust
#[tokio::test]
async fn watcher_debounces_and_reports_overflow() {
    let mut watcher = fixture_watcher(4);
    burst_create(100);
    assert!(watcher.next_batch().await.unwrap().requires_full_rescan);
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p cellar-windows --test watcher --test watcher_overflow`

Expected: FAIL.

- [ ] **Step 3: Implement a watcher event contract**

```rust
pub struct ChangeBatch {
    pub hints: Vec<ChangeHint>,
    pub requires_full_rescan: bool,
    pub observed_at: OffsetDateTime,
}

pub trait ChangeWatcher {
    async fn next_batch(&mut self) -> Result<ChangeBatch, WatchError>;
}
```

Use a bounded channel, debounce, emit overflow explicitly, and never silently drop overflow/error state. Root overflow sets the all-projects flag.

- [ ] **Step 4: Run watcher tests**

Run: `cargo test -p cellar-windows --test watcher --test watcher_overflow`

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-windows
git commit -m "feat: add bounded Windows watcher"
```

### Task 3: Implement dirty-epoch generation reconciliation

**Files:**
- Create: `crates/cellar-service/src/reconcile.rs`
- Modify: `crates/cellar-db/src/file_repo.rs`
- Test: `crates/cellar-service/tests/reconcile.rs`
- Test: `crates/cellar-service/tests/reconcile_faults.rs`

- [ ] **Step 1: Write failing fencing tests**

```rust
#[tokio::test]
async fn scan_cannot_mark_clean_when_epoch_changes() {
    let project = dirty_project(7).await;
    let scan = begin_scan(project.id).await;
    increment_dirty_epoch(project.id).await;
    scan.finish().await.unwrap();
    assert!(project_state(project.id).await.is_dirty());
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p cellar-service --test reconcile --test reconcile_faults`

Expected: FAIL.

- [ ] **Step 3: Implement reconciliation**

```rust
pub struct ScanFence {
    pub project_id: ProjectId,
    pub dirty_epoch: i64,
    pub generation: i64,
}

pub async fn finish_scan(repo: &FileRepo, fence: ScanFence) -> Result<bool, CellarError> {
    repo.mark_missing_and_clean_only_if_epoch_matches(fence).await
}
```

Increment epoch on watcher event/error/overflow and during-scan change. Upsert incrementally. Mark missing only after successful full scan with unchanged epoch. Bound retries and back off while leaving dirty visible.

- [ ] **Step 4: Run reconciliation tests**

Run: `cargo test -p cellar-service --test reconcile --test reconcile_faults`

Expected: PASS for normal hints, root overflow, scan-time changes, partial scan failure, repeated churn, and no false missing transition.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-service crates/cellar-db
git commit -m "feat: add fenced filesystem reconciliation"
```

### Task 4: Add settling and bounded hashing

**Files:**
- Create: `crates/cellar-service/src/hash_queue.rs`
- Modify: `crates/cellar-service/src/reconcile.rs`
- Test: `crates/cellar-service/tests/settling.rs`
- Test: `crates/cellar-service/tests/hash_queue.rs`

- [ ] **Step 1: Write failing stability tests**

```rust
#[tokio::test]
async fn does_not_hash_while_writer_is_active() {
    let writer = hold_incompatible_writer("large.bin");
    reconcile_once().await;
    assert_eq!(entry("large.bin").state, FileState::Settling);
    drop(writer);
    advance_stability_window();
    reconcile_once().await;
    assert_eq!(entry("large.bin").hash_state, HashState::Queued);
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p cellar-service --test settling --test hash_queue`

Expected: FAIL.

- [ ] **Step 3: Implement stability and hashing**

```rust
pub struct HashJob {
    pub entry_id: FileEntryId,
    pub expected_identity: PlatformIdentity,
    pub expected_revision: i64,
}
```

Require stable size/FILETIME across the window and no incompatible writer share. Run one bounded blocking SHA-256 worker, yield to transfers, cancel on identity/revision change, and record retryable failure.

- [ ] **Step 4: Run tests**

Run: `cargo test -p cellar-service --test settling --test hash_queue`

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-service
git commit -m "feat: settle and hash Explorer files"
```

### Task 5: Reconcile Cellar operations with watcher events

**Files:**
- Modify: `crates/cellar-service/src/reconcile.rs`
- Modify: `crates/cellar-service/src/recovery.rs`
- Test: `crates/cellar-service/tests/operation_events.rs`

- [ ] **Step 1: Write failing correlation test**

```rust
#[tokio::test]
async fn own_event_completes_operation_without_duplicate_entry() {
    let op = begin_move("a.txt", "b.txt").await;
    emit_watcher_rename(op.result_identity());
    recover_and_reconcile().await;
    assert_eq!(live_entries_named("b.txt"), 1);
    assert_eq!(operation(op.id).state, OperationState::Complete);
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p cellar-service --test operation_events`

Expected: FAIL.

- [ ] **Step 3: Implement operation-aware event merge**

```rust
pub enum EventDisposition {
    CompletesOperation(OperationId),
    ExternalChange,
    AmbiguousRequiresScan,
}
```

Match result identities and expected namespace, never ignore events by path alone, and route ambiguous matches to dirty reconciliation.

- [ ] **Step 4: Run tests**

Run: `cargo test -p cellar-service --test operation_events`

Expected: PASS for own move/copy/trash/upload, external destination steal, and ambiguous rename pairs.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-service
git commit -m "feat: correlate operation watcher events"
```

### Task 6: Implement catalog backup, restore, and recovered projects

**Files:**
- Create: `crates/cellar-db/src/backup.rs`
- Modify: `crates/cellar-service/src/cli.rs`
- Modify: `crates/cellar-service/src/reconcile.rs`
- Test: `crates/cellar-db/tests/backup_restore.rs`

- [ ] **Step 1: Write failing backup tests**

```rust
#[tokio::test]
async fn restore_imports_post_backup_project_directories() {
    let backup = create_backup().await;
    let orphan_id = create_project_after_backup().await;
    restore_backup(backup.id).await;
    assert_eq!(project(orphan_id).name, format!("Recovered {}", orphan_id.short()));
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p cellar-db --test backup_restore`

Expected: FAIL.

- [ ] **Step 3: Implement backup set lifecycle**

```rust
pub struct BackupManifest {
    pub application_version: String,
    pub schema_version: i64,
    pub storage_volume_identity: String,
    pub created_at: OffsetDateTime,
    pub db_integrity_ok: bool,
    pub config_sha256: String,
}
```

Implement `backup create/list/restore`, online SQLite backup, config copy, manifest verification, seven-set retention, quiesced restore, safe current-DB preservation, migration, full reconcile, recovered-project import, and rollback on failure.

- [ ] **Step 4: Run Phase 3 verification**

Run:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-db crates/cellar-service
git commit -m "feat: add catalog backup and restore"
```

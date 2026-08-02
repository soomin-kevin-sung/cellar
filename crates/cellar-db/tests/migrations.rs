use std::{cmp::Ordering, path::PathBuf};

use cellar_db::{DbError, FilenameCollation, migrate, open_pool};
use sqlx::{Executor, Row, SqlitePool};
use tempfile::TempDir;

struct TestDb {
    _directory: TempDir,
    path: PathBuf,
}

impl TestDb {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create temporary database directory");
        let path = directory.path().join("cellar.sqlite3");
        Self {
            _directory: directory,
            path,
        }
    }

    async fn open(&self) -> SqlitePool {
        open_pool(&self.path, test_collation())
            .await
            .expect("open database pool")
    }
}

#[cfg(windows)]
fn test_collation() -> FilenameCollation {
    FilenameCollation::windows_ordinal_ci_v1(compare_string_ordinal_ignore_case)
}

#[cfg(not(windows))]
fn test_collation() -> FilenameCollation {
    FilenameCollation::windows_ordinal_ci_v1(str::cmp)
}

#[cfg(windows)]
fn compare_string_ordinal_ignore_case(left: &str, right: &str) -> Ordering {
    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn CompareStringOrdinal(
            string1: *const u16,
            string1_len: i32,
            string2: *const u16,
            string2_len: i32,
            ignore_case: i32,
        ) -> i32;
    }

    let left: Vec<u16> = left.encode_utf16().collect();
    let right: Vec<u16> = right.encode_utf16().collect();
    // SAFETY: both pointers remain valid for their explicitly supplied lengths.
    match unsafe {
        CompareStringOrdinal(
            left.as_ptr(),
            left.len()
                .try_into()
                .expect("left filename length fits i32"),
            right.as_ptr(),
            right
                .len()
                .try_into()
                .expect("right filename length fits i32"),
            1,
        )
    } {
        1 => Ordering::Less,
        2 => Ordering::Equal,
        3 => Ordering::Greater,
        result => panic!("CompareStringOrdinal failed with result {result}"),
    }
}

async fn migrated_db() -> (TestDb, SqlitePool) {
    let db = TestDb::new();
    let pool = db.open().await;
    migrate(&pool).await.expect("run migrations");
    (db, pool)
}

async fn insert_project(pool: &SqlitePool, id: &str) {
    sqlx::query(
        "INSERT INTO project
         (id, name, status, version, created_at, updated_at)
         VALUES (?, 'Project', 'active', 1, '2026-07-31T00:00:00Z', '2026-07-31T00:00:00Z')",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("insert project");
}

async fn insert_file(
    pool: &SqlitePool,
    id: &str,
    project_id: &str,
    parent_id: Option<&str>,
    name: &str,
    state: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO file_entry
         (id, project_id, parent_id, exact_name, kind, platform_kind, size,
          mtime_filetime_100ns, hash_state, state, revision, scan_generation, observed_at)
         VALUES (?, ?, ?, ?, 'file', 'windows_file_id', 0, 0, 'unknown', ?, 1, 0,
                 '2026-07-31T00:00:00Z')",
    )
    .bind(id)
    .bind(project_id)
    .bind(parent_id)
    .bind(name)
    .bind(state)
    .execute(pool)
    .await
    .map(|_| ())
}

async fn insert_upload(
    pool: &SqlitePool,
    id: &str,
    project_id: &str,
    parent_id: Option<&str>,
    name: &str,
    state: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO upload_session
         (id, project_id, destination_parent_id, destination_name, expected_size,
          committed_offset, state, expires_at)
         VALUES (?, ?, ?, ?, 10, 0, ?, '2026-08-01T00:00:00Z')",
    )
    .bind(id)
    .bind(project_id)
    .bind(parent_id)
    .bind(name)
    .bind(state)
    .execute(pool)
    .await
    .map(|_| ())
}

#[tokio::test]
async fn migrations_are_versioned_and_safe_under_concurrent_execution() {
    let db = TestDb::new();
    let first = db.open().await;
    let second = db.open().await;

    let (left, right) = tokio::join!(migrate(&first), migrate(&second));
    left.expect("first migration succeeds");
    right.expect("second migration succeeds");

    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM cellar_schema_migration ORDER BY version")
            .fetch_all(&first)
            .await
            .expect("read migration versions");
    assert_eq!(versions, [1, 2]);

    first.close().await;
    second.close().await;
}

async fn previous_release_v2_accepts_schema_history(pool: &SqlitePool) -> bool {
    let installed: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT version, name, fingerprint
         FROM cellar_schema_migration ORDER BY version",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let known = [
        (1, "initial", "cellar-0001-initial-v3"),
        (2, "indexes", "cellar-0002-indexes-v2"),
    ];
    installed.len() <= known.len()
        && installed
            .iter()
            .zip(known)
            .all(|((version, name, fingerprint), expected)| {
                (*version, name.as_str(), fingerprint.as_str()) == expected
            })
}

#[tokio::test]
async fn expand_only_catalog_extension_remains_openable_by_previous_v2_release() {
    let (_db, pool) = migrated_db().await;
    assert!(previous_release_v2_accepts_schema_history(&pool).await);
    let marker: String = sqlx::query_scalar(
        "SELECT fingerprint FROM cellar_schema_extension
         WHERE name = 'file_catalog_epoch'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(marker, "cellar-file-catalog-epoch-v1");
    let epoch_table: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master
         WHERE type = 'table' AND name = 'file_catalog_epoch'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(epoch_table, 1);
    pool.close().await;
}

#[tokio::test]
async fn expand_only_upload_cleanup_queue_is_durable_and_v2_compatible() {
    let (_db, pool) = migrated_db().await;
    assert!(previous_release_v2_accepts_schema_history(&pool).await);
    let marker: String = sqlx::query_scalar(
        "SELECT fingerprint FROM cellar_schema_extension
         WHERE name = 'upload_staging_cleanup'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(marker, "cellar-upload-staging-cleanup-v1");
    insert_project(&pool, "p1").await;
    insert_upload(&pool, "cleanup", "p1", None, "cleanup.bin", "created")
        .await
        .unwrap();
    sqlx::query("UPDATE upload_session SET state = 'cancelled' WHERE id = 'cleanup'")
        .execute(&pool)
        .await
        .unwrap();
    let queued: String = sqlx::query_scalar(
        "SELECT upload_id FROM upload_staging_cleanup WHERE upload_id = 'cleanup'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(queued, "cleanup");
    pool.close().await;
}

#[tokio::test]
async fn upload_cleanup_extension_tampering_fails_closed() {
    let (_marker_db, marker_pool) = migrated_db().await;
    sqlx::query(
        "UPDATE cellar_schema_extension SET fingerprint = 'tampered'
         WHERE name = 'upload_staging_cleanup'",
    )
    .execute(&marker_pool)
    .await
    .unwrap();
    assert!(matches!(
        migrate(&marker_pool).await,
        Err(DbError::SchemaVersion)
    ));
    assert!(previous_release_v2_accepts_schema_history(&marker_pool).await);
    marker_pool.close().await;

    let (_object_db, object_pool) = migrated_db().await;
    sqlx::raw_sql(
        "DROP TRIGGER upload_staging_cleanup_terminal;
         CREATE TRIGGER upload_staging_cleanup_terminal
         AFTER UPDATE OF state ON upload_session BEGIN SELECT 1; END;",
    )
    .execute(&object_pool)
    .await
    .unwrap();
    assert!(matches!(
        migrate(&object_pool).await,
        Err(DbError::SchemaVersion)
    ));
    assert!(previous_release_v2_accepts_schema_history(&object_pool).await);
    object_pool.close().await;
}

#[tokio::test]
async fn catalog_extension_marker_tampering_fails_closed_without_new_history_rows() {
    let (_db, pool) = migrated_db().await;
    sqlx::query(
        "UPDATE cellar_schema_extension SET fingerprint = 'tampered'
         WHERE name = 'file_catalog_epoch'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(migrate(&pool).await, Err(DbError::SchemaVersion)));
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM cellar_schema_migration ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(versions, [1, 2]);
    pool.close().await;
}

#[tokio::test]
async fn file_catalog_epochs_are_project_scoped_and_cover_every_row_mutation() {
    let (_db, pool) = migrated_db().await;
    insert_project(&pool, "p1").await;
    insert_project(&pool, "p2").await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT version FROM file_catalog_epoch WHERE project_id = 'p1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );

    insert_file(&pool, "entry", "p1", None, "a", "live")
        .await
        .unwrap();
    sqlx::query("UPDATE file_entry SET exact_name = 'b' WHERE id = 'entry'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM file_entry WHERE id = 'entry'")
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT version FROM file_catalog_epoch WHERE project_id = 'p1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        3
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT version FROM file_catalog_epoch WHERE project_id = 'p2'",
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );

    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master
         WHERE type = 'index' AND name LIKE 'ix_file_list_%' ORDER BY name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(indexes, ["ix_file_list_child", "ix_file_list_root"]);
    pool.close().await;
}

#[tokio::test]
async fn migrations_reject_an_unknown_schema_version_without_leaking_sql() {
    let (_db, pool) = migrated_db().await;
    sqlx::query(
        "INSERT INTO cellar_schema_migration (version, name, fingerprint)
         VALUES (99, 'future', 'future')",
    )
    .execute(&pool)
    .await
    .expect("insert future schema marker");

    let error = migrate(&pool)
        .await
        .expect_err("future schema must be rejected");
    assert!(matches!(error, DbError::SchemaVersion));
    assert_eq!(error.to_string(), "database schema version is unsupported");
    pool.close().await;
}

#[tokio::test]
async fn open_and_migrate_reject_a_collation_metadata_mismatch() {
    let (db, pool) = migrated_db().await;
    sqlx::query(
        "UPDATE cellar_schema_metadata
         SET value = '2'
         WHERE key = 'filename_collation_version'",
    )
    .execute(&pool)
    .await
    .expect("replace collation metadata");

    let migration_error = migrate(&pool)
        .await
        .expect_err("migration must reject mismatched collation metadata");
    assert!(matches!(migration_error, DbError::SchemaVersion));
    pool.close().await;

    let open_error = open_pool(&db.path, test_collation())
        .await
        .expect_err("open must reject mismatched collation metadata");
    assert!(matches!(open_error, DbError::SchemaVersion));
}

#[tokio::test]
async fn a_failed_second_migration_rolls_back_earlier_index_creation() {
    let db = TestDb::new();
    let pool = db.open().await;
    sqlx::raw_sql(include_str!("../../../migrations/0001_initial.sql"))
        .execute(&pool)
        .await
        .expect("install version-one schema");
    sqlx::query(
        "CREATE TABLE cellar_schema_migration (
           version INTEGER PRIMARY KEY NOT NULL,
           name TEXT NOT NULL,
           fingerprint TEXT NOT NULL
         )",
    )
    .execute(&pool)
    .await
    .expect("create migration history");
    sqlx::query(
        "INSERT INTO cellar_schema_migration (version, name, fingerprint)
         VALUES (1, 'initial', 'cellar-0001-initial-v3')",
    )
    .execute(&pool)
    .await
    .expect("record version one");
    insert_project(&pool, "p1").await;
    insert_file(&pool, "dir", "p1", None, "dir", "live")
        .await
        .expect("insert parent");
    insert_file(&pool, "child-a", "p1", Some("dir"), "duplicate", "live")
        .await
        .expect("insert first duplicate");
    insert_file(&pool, "child-b", "p1", Some("dir"), "duplicate", "live")
        .await
        .expect("insert second duplicate before indexes exist");

    assert!(matches!(migrate(&pool).await, Err(DbError::Migration(_))));

    let root_index_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master
         WHERE type = 'index' AND name = 'uq_file_root_name'",
    )
    .fetch_one(&pool)
    .await
    .expect("inspect rolled-back index");
    let version_two_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cellar_schema_migration WHERE version = 2")
            .fetch_one(&pool)
            .await
            .expect("inspect migration history");
    assert_eq!(root_index_count, 0);
    assert_eq!(version_two_count, 0);
    pool.close().await;
}

#[tokio::test]
async fn every_pool_connection_has_required_sqlite_pragmas() {
    let db = TestDb::new();
    let pool = db.open().await;
    let mut connections = Vec::new();
    for _ in 0..4 {
        connections.push(pool.acquire().await.expect("acquire pooled connection"));
    }

    for connection in &mut connections {
        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&mut **connection)
            .await
            .expect("read journal mode");
        let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&mut **connection)
            .await
            .expect("read synchronous");
        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&mut **connection)
            .await
            .expect("read foreign keys");
        let busy_timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(&mut **connection)
            .await
            .expect("read busy timeout");

        assert_eq!(journal_mode, "wal");
        assert_eq!(synchronous, 2);
        assert_eq!(foreign_keys, 1);
        assert_eq!(busy_timeout, 5_000);
    }

    drop(connections);
    pool.close().await;
}

#[tokio::test]
async fn foreign_keys_reject_cross_project_parents() {
    let (_db, pool) = migrated_db().await;
    insert_project(&pool, "p1").await;
    insert_project(&pool, "p2").await;
    insert_file(&pool, "parent", "p1", None, "parent", "live")
        .await
        .expect("insert parent");

    assert!(
        insert_file(&pool, "child", "p2", Some("parent"), "child", "live")
            .await
            .is_err()
    );
    pool.close().await;
}

#[tokio::test]
async fn all_state_columns_reject_unknown_values() {
    let (_db, pool) = migrated_db().await;
    insert_project(&pool, "p1").await;

    let statements = [
        "INSERT INTO project (id, name, status, version, created_at, updated_at)
         VALUES ('bad-project', 'x', 'unknown', 1, 't', 't')",
        "INSERT INTO file_entry
         (id, project_id, exact_name, kind, platform_kind, size, mtime_filetime_100ns,
          hash_state, state, revision, scan_generation, observed_at)
         VALUES ('bad-file-state', 'p1', 'a', 'file', 'x', 0, 0, 'unknown',
                 'unknown', 1, 0, 't')",
        "INSERT INTO file_entry
         (id, project_id, exact_name, kind, platform_kind, size, mtime_filetime_100ns,
          hash_state, state, revision, scan_generation, observed_at)
         VALUES ('bad-file-kind', 'p1', 'b', 'symlink', 'x', 0, 0, 'unknown',
                 'live', 1, 0, 't')",
        "INSERT INTO file_entry
         (id, project_id, exact_name, kind, platform_kind, size, mtime_filetime_100ns,
          hash_state, state, revision, scan_generation, observed_at)
         VALUES ('bad-hash-state', 'p1', 'c', 'file', 'x', 0, 0, 'invalid',
                 'live', 1, 0, 't')",
        "INSERT INTO upload_session
         (id, project_id, destination_name, expected_size, committed_offset, state, expires_at)
         VALUES ('bad-upload', 'p1', 'a', 0, 0, 'unknown', 't')",
        "INSERT INTO operation
         (id, project_id, kind, state, payload_version, payload, created_at, updated_at)
         VALUES ('bad-operation', 'p1', 'x', 'unknown', 1, '{}', 't', 't')",
        "INSERT INTO trash_item
         (id, project_id, original_path_snapshot, storage_path, deleted_at, purge_after, state)
         VALUES ('bad-trash', 'p1', 'a', 'store-a', 't', 't', 'unknown')",
        "INSERT INTO audit_event
         (event_id, source, action, result, occurred_at, details)
         VALUES ('bad-audit', 'unknown', 'x', 'x', 't', '{}')",
    ];

    for statement in statements {
        assert!(
            pool.execute(statement).await.is_err(),
            "constraint unexpectedly accepted: {statement}"
        );
    }
    pool.close().await;
}

#[tokio::test]
async fn numeric_versions_sizes_offsets_and_hash_lengths_are_checked() {
    let (_db, pool) = migrated_db().await;
    insert_project(&pool, "p1").await;

    let statements = [
        "INSERT INTO project (id, name, status, version, created_at, updated_at)
         VALUES ('version-zero', 'x', 'active', 0, 't', 't')",
        "INSERT INTO file_entry
         (id, project_id, exact_name, kind, platform_kind, size, mtime_filetime_100ns,
          hash_state, state, revision, scan_generation, observed_at)
         VALUES ('size-negative', 'p1', 'a', 'file', 'x', -1, 0, 'unknown',
                 'live', 1, 0, 't')",
        "INSERT INTO file_entry
         (id, project_id, exact_name, kind, platform_kind, size, mtime_filetime_100ns,
          hash_state, state, revision, scan_generation, observed_at)
         VALUES ('revision-zero', 'p1', 'b', 'file', 'x', 0, 0, 'unknown',
                 'live', 0, 0, 't')",
        "INSERT INTO file_entry
         (id, project_id, exact_name, kind, platform_kind, size, mtime_filetime_100ns,
          hash, hash_state, state, revision, scan_generation, observed_at)
         VALUES ('short-hash', 'p1', 'c', 'file', 'x', 0, 0, x'00', 'ready',
                 'live', 1, 0, 't')",
        "INSERT INTO upload_session
         (id, project_id, destination_name, expected_size, committed_offset, state, expires_at)
         VALUES ('negative-size', 'p1', 'u1', -1, 0, 'created', 't')",
        "INSERT INTO upload_session
         (id, project_id, destination_name, expected_size, committed_offset, state, expires_at)
         VALUES ('negative-offset', 'p1', 'u2', 1, -1, 'created', 't')",
        "INSERT INTO upload_session
         (id, project_id, destination_name, expected_size, committed_offset, state, expires_at)
         VALUES ('large-offset', 'p1', 'u3', 1, 2, 'created', 't')",
        "INSERT INTO upload_session
         (id, project_id, destination_name, expected_size, committed_offset, expected_hash,
          state, expires_at)
         VALUES ('short-expected-hash', 'p1', 'u4', 1, 0, x'00', 'created', 't')",
        "INSERT INTO operation
         (id, kind, state, payload_version, payload, created_at, updated_at)
         VALUES ('bad-payload-version', 'x', 'pending', 0, '{}', 't', 't')",
    ];

    for statement in statements {
        assert!(
            pool.execute(statement).await.is_err(),
            "constraint unexpectedly accepted: {statement}"
        );
    }
    pool.close().await;
}

#[tokio::test]
async fn live_root_and_child_names_are_unique_but_inactive_names_are_reusable() {
    let (_db, pool) = migrated_db().await;
    insert_project(&pool, "p1").await;
    insert_file(&pool, "root-a", "p1", None, "same.txt", "live")
        .await
        .expect("insert live root");
    assert!(
        insert_file(&pool, "root-b", "p1", None, "same.txt", "settling")
            .await
            .is_err()
    );
    insert_file(&pool, "root-c", "p1", None, "same.txt", "missing")
        .await
        .expect("inactive root name may be reused");

    insert_file(&pool, "dir", "p1", None, "dir", "live")
        .await
        .expect("insert directory-shaped parent");
    insert_file(&pool, "child-a", "p1", Some("dir"), "same.txt", "settling")
        .await
        .expect("insert settling child");
    assert!(
        insert_file(&pool, "child-b", "p1", Some("dir"), "same.txt", "live")
            .await
            .is_err()
    );
    insert_file(&pool, "child-c", "p1", Some("dir"), "same.txt", "trashed")
        .await
        .expect("inactive child name may be reused");
    pool.close().await;
}

#[cfg(windows)]
#[tokio::test]
async fn filename_uniqueness_uses_windows_ordinal_ignore_case() {
    let (_db, pool) = migrated_db().await;
    insert_project(&pool, "p1").await;
    let sqlite_nocase_equal: i64 =
        sqlx::query_scalar("SELECT 'RÉSUMÉ.TXT' = 'résumé.txt' COLLATE NOCASE")
            .fetch_one(&pool)
            .await
            .expect("compare with SQLite NOCASE");
    assert_eq!(sqlite_nocase_equal, 0);
    insert_file(&pool, "a", "p1", None, "RÉSUMÉ.TXT", "live")
        .await
        .expect("insert first spelling");

    assert!(
        insert_file(&pool, "b", "p1", None, "résumé.txt", "live")
            .await
            .is_err()
    );
    pool.close().await;
}

#[tokio::test]
async fn malformed_utf8_text_uses_deterministic_raw_byte_ordering() {
    let db = TestDb::new();
    let pool = db.open().await;

    let forward: i64 = sqlx::query_scalar(
        "SELECT CAST(x'80' AS TEXT) COLLATE WINDOWS_ORDINAL_CI_V1
                < CAST(x'81' AS TEXT)",
    )
    .fetch_one(&pool)
    .await
    .expect("compare malformed UTF-8 in byte order");
    let reverse: i64 = sqlx::query_scalar(
        "SELECT CAST(x'81' AS TEXT) COLLATE WINDOWS_ORDINAL_CI_V1
                < CAST(x'80' AS TEXT)",
    )
    .fetch_one(&pool)
    .await
    .expect("compare malformed UTF-8 in reverse byte order");
    assert_eq!((forward, reverse), (1, 0));
    pool.close().await;
}

#[tokio::test]
async fn malformed_text_is_disjoint_from_valid_equivalence_classes() {
    let db = TestDb::new();
    let collation = FilenameCollation::windows_ordinal_ci_v1(|left, right| {
        left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase())
    });
    let pool = open_pool(&db.path, collation)
        .await
        .expect("open pool with case-insensitive test comparator");

    let (valid_equal, raw_between, malformed_before_upper, malformed_before_lower): (
        i64,
        i64,
        i64,
        i64,
    ) = sqlx::query_as(
        "SELECT
               'A' = 'a' COLLATE WINDOWS_ORDINAL_CI_V1,
               ('A' COLLATE BINARY < CAST(x'5080' AS TEXT)
                AND CAST(x'5080' AS TEXT) COLLATE BINARY < 'a'),
               CAST(x'5080' AS TEXT) COLLATE WINDOWS_ORDINAL_CI_V1 < 'A',
               CAST(x'5080' AS TEXT) COLLATE WINDOWS_ORDINAL_CI_V1 < 'a'",
    )
    .fetch_one(&pool)
    .await
    .expect("compare malformed and equivalent valid values");
    assert_eq!(valid_equal, 1);
    assert_eq!(raw_between, 1);
    assert_eq!((malformed_before_upper, malformed_before_lower), (1, 1));

    let invalid_order: i64 = sqlx::query_scalar(
        "SELECT CAST(x'5080' AS TEXT) COLLATE WINDOWS_ORDINAL_CI_V1
                < CAST(x'5180' AS TEXT)",
    )
    .fetch_one(&pool)
    .await
    .expect("compare two malformed values by raw bytes");
    assert_eq!(invalid_order, 1);
    pool.close().await;
}

#[tokio::test]
async fn comparator_panic_interrupts_write_and_poisons_connection() {
    const CHILD_ENV: &str = "CELLAR_DB_PANIC_COLLATION_CHILD";
    if std::env::var_os(CHILD_ENV).is_none() {
        let status = std::process::Command::new(
            std::env::current_exe().expect("locate migration test executable"),
        )
        .arg("--exact")
        .arg("comparator_panic_interrupts_write_and_poisons_connection")
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .status()
        .expect("run panicking comparator subprocess");
        assert!(status.success(), "collation subprocess aborted: {status}");
        return;
    }

    let db = TestDb::new();
    let previous_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    struct PanicAgainOnDrop;
    impl Drop for PanicAgainOnDrop {
        fn drop(&mut self) {
            panic!("panic payload destructor must never run");
        }
    }
    let collation = FilenameCollation::windows_ordinal_ci_v1(|left, right| {
        if left == "panic" || right == "panic" {
            std::panic::panic_any(PanicAgainOnDrop);
        }
        left.cmp(right)
    });
    let pool = open_pool(&db.path, collation)
        .await
        .expect("open pool with panicking comparator");
    let mut connection = pool.acquire().await.expect("acquire one connection");
    sqlx::query(
        "CREATE TABLE panic_write (
           name TEXT NOT NULL COLLATE WINDOWS_ORDINAL_CI_V1 UNIQUE
         )",
    )
    .execute(&mut *connection)
    .await
    .expect("create selective-panic table");
    sqlx::query("INSERT INTO panic_write (name) VALUES ('alpha')")
        .execute(&mut *connection)
        .await
        .expect("insert non-panicking seed");

    let panic_insert = sqlx::query("INSERT INTO panic_write (name) VALUES ('panic')")
        .execute(&mut *connection)
        .await;
    assert!(
        panic_insert.is_err(),
        "panicking comparison committed a row"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM panic_write")
        .fetch_one(&mut *connection)
        .await
        .expect("count rows after interrupted insert");
    assert_eq!(count, 1);

    let poisoned_insert = sqlx::query("INSERT INTO panic_write (name) VALUES ('bravo')")
        .execute(&mut *connection)
        .await;
    assert!(
        poisoned_insert.is_err(),
        "poisoned connection accepted a later indexed write"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM panic_write")
        .fetch_one(&mut *connection)
        .await
        .expect("count rows after poisoned insert");
    assert_eq!(count, 1);
    drop(connection);
    pool.close().await;
    std::panic::set_hook(previous_panic_hook);
}

#[tokio::test]
async fn active_uploads_reserve_root_and_child_destinations() {
    let (_db, pool) = migrated_db().await;
    insert_project(&pool, "p1").await;
    insert_file(&pool, "dir", "p1", None, "dir", "live")
        .await
        .expect("insert parent");

    insert_upload(&pool, "root-a", "p1", None, "target.bin", "created")
        .await
        .expect("reserve root");
    assert!(
        insert_upload(&pool, "root-b", "p1", None, "target.bin", "committing")
            .await
            .is_err()
    );
    insert_upload(&pool, "root-c", "p1", None, "target.bin", "complete")
        .await
        .expect("completed reservation is released");

    insert_upload(
        &pool,
        "child-a",
        "p1",
        Some("dir"),
        "target.bin",
        "uploading",
    )
    .await
    .expect("reserve child");
    assert!(
        insert_upload(
            &pool,
            "child-b",
            "p1",
            Some("dir"),
            "target.bin",
            "verifying",
        )
        .await
        .is_err()
    );
    insert_upload(
        &pool,
        "child-c",
        "p1",
        Some("dir"),
        "target.bin",
        "cancelled",
    )
    .await
    .expect("cancelled reservation is released");
    pool.close().await;
}

#[tokio::test]
async fn project_cover_requires_same_project_and_is_cleared_when_file_is_removed() {
    let (_db, pool) = migrated_db().await;
    insert_project(&pool, "p1").await;
    insert_project(&pool, "p2").await;
    insert_file(&pool, "cover", "p1", None, "cover.jpg", "live")
        .await
        .expect("insert cover");

    assert!(
        sqlx::query("INSERT INTO project_cover (project_id, file_entry_id) VALUES ('p2', 'cover')")
            .execute(&pool)
            .await
            .is_err()
    );
    sqlx::query("INSERT INTO project_cover (project_id, file_entry_id) VALUES ('p1', 'cover')")
        .execute(&pool)
        .await
        .expect("insert matching cover");
    sqlx::query("DELETE FROM file_entry WHERE id = 'cover'")
        .execute(&pool)
        .await
        .expect("delete covered file");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM project_cover")
        .fetch_one(&pool)
        .await
        .expect("count covers");
    assert_eq!(count, 0);
    pool.close().await;
}

#[tokio::test]
async fn pending_chunks_are_all_null_or_all_present_and_in_bounds() {
    let (_db, pool) = migrated_db().await;
    insert_project(&pool, "p1").await;

    sqlx::query(
        "INSERT INTO upload_session
         (id, project_id, destination_name, expected_size, committed_offset, state, expires_at)
         VALUES ('none', 'p1', 'none', 10, 0, 'uploading', 't')",
    )
    .execute(&pool)
    .await
    .expect("all-null pending chunk");
    sqlx::query(
        "INSERT INTO upload_session
         (id, project_id, destination_name, expected_size, committed_offset,
          pending_offset, pending_length, pending_digest, state, expires_at)
         VALUES ('present', 'p1', 'present', 10, 2, 2, 8, zeroblob(32), 'uploading', 't')",
    )
    .execute(&pool)
    .await
    .expect("valid pending chunk");

    let invalid = [
        "VALUES ('partial', 'p1', 'partial', 10, 0, 0, NULL, NULL, 'uploading', 't')",
        "VALUES ('wrong-offset', 'p1', 'wrong-offset', 10, 2, 3, 1, zeroblob(32), 'uploading', 't')",
        "VALUES ('zero-length', 'p1', 'zero-length', 10, 0, 0, 0, zeroblob(32), 'uploading', 't')",
        "VALUES ('out-of-bounds', 'p1', 'out-of-bounds', 10, 8, 8, 3, zeroblob(32), 'uploading', 't')",
        "VALUES ('short-digest', 'p1', 'short-digest', 10, 0, 0, 1, x'00', 'uploading', 't')",
        "VALUES ('overflow', 'p1', 'overflow', 9223372036854775807,
                 9223372036854775807, 9223372036854775807, 1, zeroblob(32),
                 'uploading', 't')",
    ];
    for values in invalid {
        let statement = format!(
            "INSERT INTO upload_session
             (id, project_id, destination_name, expected_size, committed_offset,
              pending_offset, pending_length, pending_digest, state, expires_at)
             {values}"
        );
        assert!(
            pool.execute(statement.as_str()).await.is_err(),
            "constraint unexpectedly accepted {values}"
        );
    }
    pool.close().await;
}

#[tokio::test]
async fn audit_events_do_not_cascade_with_projects() {
    let (_db, pool) = migrated_db().await;
    insert_project(&pool, "p1").await;
    sqlx::query(
        "INSERT INTO audit_event
         (event_id, source, project_id, action, result, occurred_at, details)
         VALUES ('event', 'web', 'p1', 'create', 'ok', 't', '{}')",
    )
    .execute(&pool)
    .await
    .expect("insert audit event");

    assert!(
        sqlx::query("DELETE FROM project WHERE id = 'p1'")
            .execute(&pool)
            .await
            .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event")
        .fetch_one(&pool)
        .await
        .expect("count audit events");
    assert_eq!(count, 1);
    pool.close().await;
}

#[tokio::test]
async fn schema_contains_all_descriptive_columns() {
    let (_db, pool) = migrated_db().await;
    let expected = [
        (
            "project",
            "id,name,description,status,version,created_at,updated_at,deleted_at",
        ),
        (
            "file_entry",
            "id,project_id,parent_id,exact_name,kind,platform_kind,volume_serial,filesystem_file_id,size,mtime_filetime_100ns,hash,hash_state,state,revision,scan_generation,observed_at",
        ),
        ("project_cover", "project_id,file_entry_id"),
        (
            "upload_session",
            "id,project_id,destination_parent_id,destination_name,expected_size,committed_offset,expected_hash,pending_offset,pending_length,pending_digest,state,expires_at",
        ),
        (
            "operation",
            "id,project_id,kind,state,payload_version,payload,error,created_at,updated_at",
        ),
        (
            "trash_item",
            "id,project_id,root_entry_id,original_path_snapshot,storage_path,deleted_at,purge_after,state",
        ),
        (
            "audit_event",
            "sequence,event_id,operation_id,source,project_id,target_id,path_snapshot,action,result,occurred_at,details",
        ),
    ];

    for (table, column_list) in expected {
        let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
            .fetch_all(&pool)
            .await
            .expect("read table columns");
        let actual: Vec<String> = rows
            .iter()
            .map(|row| row.get::<String, _>("name"))
            .collect();
        let expected: Vec<&str> = column_list.split(',').collect();
        assert_eq!(actual, expected, "columns for {table}");
    }
    pool.close().await;
}

#[tokio::test]
async fn filename_indexes_explicitly_pin_collation_version_one() {
    let (_db, pool) = migrated_db().await;
    let index_sql: Vec<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master
         WHERE type = 'index'
           AND name IN ('uq_file_root_name', 'uq_file_child_name',
                        'uq_upload_root_destination', 'uq_upload_child_destination')
         ORDER BY name",
    )
    .fetch_all(&pool)
    .await
    .expect("read filename index DDL");
    assert_eq!(index_sql.len(), 4);
    assert!(
        index_sql
            .iter()
            .all(|sql| sql.contains("COLLATE WINDOWS_ORDINAL_CI_V1"))
    );
    pool.close().await;
}

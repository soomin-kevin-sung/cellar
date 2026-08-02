use std::sync::Arc;

use cellar_core::{FileEntryId, FileListRequest, FileRepository, FileRepositoryError, ProjectId};
use cellar_db::{FilenameCollation, SqliteFileRepository, migrate, open_pool};
use sqlx::SqlitePool;
use tempfile::TempDir;

struct TestDb {
    _directory: TempDir,
    pool: SqlitePool,
}

impl TestDb {
    async fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let collation = FilenameCollation::windows_ordinal_ci_v1(|left, right| {
            left.to_lowercase().cmp(&right.to_lowercase())
        });
        let pool = open_pool(directory.path().join("cellar.db"), collation)
            .await
            .unwrap();
        migrate(&pool).await.unwrap();
        Self {
            _directory: directory,
            pool,
        }
    }

    fn repository(&self) -> Arc<SqliteFileRepository> {
        Arc::new(SqliteFileRepository::new(self.pool.clone()))
    }
}

async fn insert_project(pool: &SqlitePool, status: &str) -> ProjectId {
    let id = ProjectId::new();
    sqlx::query(
        "INSERT INTO project
         (id, name, description, status, version, created_at, updated_at)
         VALUES (?, 'Files', '', ?, 1, '2026-07-31T00:00:00Z', '2026-07-31T00:00:00Z')",
    )
    .bind(id.to_string())
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn insert_entry(
    pool: &SqlitePool,
    project_id: ProjectId,
    parent_id: Option<FileEntryId>,
    exact_name: &str,
    kind: &str,
    state: &str,
) -> FileEntryId {
    let id = FileEntryId::new();
    insert_entry_with_id(pool, id, project_id, parent_id, exact_name, kind, state).await;
    id
}

async fn insert_entry_with_id(
    pool: &SqlitePool,
    id: FileEntryId,
    project_id: ProjectId,
    parent_id: Option<FileEntryId>,
    exact_name: &str,
    kind: &str,
    state: &str,
) {
    sqlx::query(
        "INSERT INTO file_entry
         (id, project_id, parent_id, exact_name, kind, platform_kind, volume_serial,
          filesystem_file_id, size, mtime_filetime_100ns, hash, hash_state, state,
          revision, scan_generation, observed_at)
         VALUES (?, ?, ?, ?, ?, 'windows_file_id', ?, ?, 12, 1337, NULL, 'unknown',
                 ?, 1, 7, '2026-07-31T00:00:00Z')",
    )
    .bind(id.to_string())
    .bind(project_id.to_string())
    .bind(parent_id.map(|id| id.to_string()))
    .bind(exact_name)
    .bind(kind)
    .bind([1_u8; 8].as_slice())
    .bind(id.to_string().as_bytes()[..16].to_vec())
    .bind(state)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn exact_windows_ordering_uses_binary_uuid_tie_breaker_without_gaps() {
    let db = TestDb::new().await;
    let project = insert_project(&db.pool, "active").await;
    let ids = [FileEntryId::new(), FileEntryId::new()];
    insert_entry_with_id(
        &db.pool,
        ids[0],
        project,
        None,
        "same",
        "file",
        "unsupported",
    )
    .await;
    insert_entry_with_id(
        &db.pool,
        ids[1],
        project,
        None,
        "SAME",
        "file",
        "unsupported",
    )
    .await;
    insert_entry(&db.pool, project, None, "zeta", "file", "live").await;

    let repository = db.repository();
    let first = repository
        .list(FileListRequest::first(project, None, 1).unwrap())
        .await
        .unwrap();
    let second = repository
        .list(FileListRequest::after(first.next_cursor.clone().unwrap(), 1).unwrap())
        .await
        .unwrap();
    let third = repository
        .list(FileListRequest::after(second.next_cursor.clone().unwrap(), 1).unwrap())
        .await
        .unwrap();
    let listed = [first.items[0].id, second.items[0].id];
    let mut expected = ids;
    expected.sort_by_key(ToString::to_string);
    assert_eq!(listed, expected);
    assert_eq!(third.items[0].exact_name.as_str(), "zeta");
    assert!(third.next_cursor.is_none());
}

#[tokio::test]
async fn root_child_and_visible_state_filters_are_exact() {
    let db = TestDb::new().await;
    let project = insert_project(&db.pool, "active").await;
    let folder = insert_entry(&db.pool, project, None, "folder", "directory", "live").await;
    for state in ["live", "settling", "unsupported", "missing", "trashed"] {
        insert_entry(
            &db.pool,
            project,
            Some(folder),
            &format!("{state}.txt"),
            "file",
            state,
        )
        .await;
    }
    insert_entry(&db.pool, project, None, "root.txt", "file", "live").await;

    let repository = db.repository();
    let root = repository
        .list(FileListRequest::first(project, None, 100).unwrap())
        .await
        .unwrap();
    assert_eq!(
        root.items
            .iter()
            .map(|entry| entry.exact_name.as_str())
            .collect::<Vec<_>>(),
        ["folder", "root.txt"]
    );
    let child = repository
        .list(FileListRequest::first(project, Some(folder), 100).unwrap())
        .await
        .unwrap();
    assert_eq!(
        child
            .items
            .iter()
            .map(|entry| entry.exact_name.as_str())
            .collect::<Vec<_>>(),
        ["live.txt", "settling.txt", "unsupported.txt"]
    );
    assert!(
        child
            .items
            .iter()
            .all(|entry| entry.relative_path.starts_with("folder/"))
    );
}

#[tokio::test]
async fn project_scoped_epoch_rejects_changed_snapshots_but_not_other_projects() {
    let db = TestDb::new().await;
    let project = insert_project(&db.pool, "active").await;
    let other = insert_project(&db.pool, "active").await;
    insert_entry(&db.pool, project, None, "a", "file", "live").await;
    insert_entry(&db.pool, project, None, "b", "file", "live").await;
    let repository = db.repository();
    let first = repository
        .list(FileListRequest::first(project, None, 1).unwrap())
        .await
        .unwrap();

    insert_entry(&db.pool, other, None, "unrelated", "file", "live").await;
    assert!(
        repository
            .list(FileListRequest::after(first.next_cursor.clone().unwrap(), 1).unwrap())
            .await
            .is_ok()
    );
    sqlx::query(
        "UPDATE file_entry SET exact_name = 'renamed' WHERE project_id = ? AND exact_name = 'b'",
    )
    .bind(project.to_string())
    .execute(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        repository
            .list(FileListRequest::after(first.next_cursor.unwrap(), 1).unwrap())
            .await,
        Err(FileRepositoryError::SnapshotChanged)
    );
}

#[tokio::test]
async fn archived_projects_are_readable_but_deleted_and_invalid_folders_fail_closed() {
    let db = TestDb::new().await;
    let archived = insert_project(&db.pool, "archived").await;
    insert_entry(&db.pool, archived, None, "archive.txt", "file", "live").await;
    assert_eq!(
        db.repository()
            .list(FileListRequest::first(archived, None, 100).unwrap())
            .await
            .unwrap()
            .items
            .len(),
        1
    );

    let active = insert_project(&db.pool, "active").await;
    let file = insert_entry(&db.pool, active, None, "plain", "file", "live").await;
    assert_eq!(
        db.repository()
            .list(FileListRequest::first(active, Some(file), 100).unwrap())
            .await,
        Err(FileRepositoryError::InvalidFolder)
    );
    sqlx::query("UPDATE project SET deleted_at = '2026-08-01T00:00:00Z' WHERE id = ?")
        .bind(active.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        db.repository()
            .list(FileListRequest::first(active, None, 100).unwrap())
            .await,
        Err(FileRepositoryError::ProjectNotFound)
    );
}

#[tokio::test]
async fn unsupported_folders_are_visible_but_cannot_be_traversed() {
    let db = TestDb::new().await;
    let project = insert_project(&db.pool, "active").await;
    let folder = insert_entry(
        &db.pool,
        project,
        None,
        "unsafe-folder",
        "directory",
        "unsupported",
    )
    .await;
    assert_eq!(
        db.repository()
            .list(FileListRequest::first(project, Some(folder), 100).unwrap())
            .await,
        Err(FileRepositoryError::UnsupportedFolder)
    );
}

#[tokio::test]
async fn malformed_catalog_rows_fail_closed() {
    let db = TestDb::new().await;
    let project = insert_project(&db.pool, "active").await;
    sqlx::query(
        "INSERT INTO file_entry
         (id, project_id, exact_name, kind, platform_kind, size, mtime_filetime_100ns,
          hash_state, state, revision, scan_generation, observed_at)
         VALUES ('not-a-uuid', ?, 'bad/name', 'file', 'x', 0, 0, 'unknown',
                 'live', 1, 0, 'not-a-time')",
    )
    .bind(project.to_string())
    .execute(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        db.repository()
            .list(FileListRequest::first(project, None, 100).unwrap())
            .await,
        Err(FileRepositoryError::Unavailable)
    );
}

use std::collections::HashSet;

use async_trait::async_trait;
use cellar_core::{
    FileCursor, FileEntry, FileEntryId, FileExactName, FileHashState, FileKind, FileListRequest,
    FilePage, FileRepository, FileRepositoryError, FileState, PlatformIdentity, ProjectId,
};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const MAX_DIRECTORY_DEPTH: usize = 256;
const MAX_RELATIVE_PATH_BYTES: usize = 128 * 1024;

#[derive(Clone)]
pub struct SqliteFileRepository {
    pool: SqlitePool,
}

impl SqliteFileRepository {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl FileRepository for SqliteFileRepository {
    async fn list(&self, request: FileListRequest) -> Result<FilePage, FileRepositoryError> {
        if request.cursor().is_some_and(|cursor| {
            cursor.project_id() != request.project_id() || cursor.parent_id() != request.parent_id()
        }) {
            return Err(FileRepositoryError::Unavailable);
        }

        let mut transaction = self.pool.begin().await.map_err(map_sql)?;
        validate_project(&mut transaction, request.project_id()).await?;
        let snapshot_version: i64 =
            sqlx::query_scalar("SELECT version FROM file_catalog_epoch WHERE project_id = ?")
                .bind(request.project_id().to_string())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sql)?
                .ok_or(FileRepositoryError::Unavailable)?;
        if snapshot_version < 0 {
            return Err(FileRepositoryError::Unavailable);
        }
        if request
            .cursor()
            .is_some_and(|cursor| cursor.snapshot_version() != snapshot_version)
        {
            return Err(FileRepositoryError::SnapshotChanged);
        }
        let parent_path = match request.parent_id() {
            Some(parent_id) => {
                directory_path(&mut transaction, request.project_id(), parent_id).await?
            }
            None => String::new(),
        };

        let fetch_limit = i64::from(request.limit()) + 1;
        let rows = fetch_rows(
            &mut transaction,
            request.project_id(),
            request.parent_id(),
            request.cursor(),
            fetch_limit,
        )
        .await?;
        let mut items = rows
            .into_iter()
            .map(|row| row_to_entry(row, request.project_id(), request.parent_id(), &parent_path))
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = items.len() > request.limit() as usize;
        if has_more {
            items.truncate(request.limit() as usize);
        }
        let next_cursor = if has_more {
            let last = items.last().ok_or(FileRepositoryError::Unavailable)?;
            Some(
                FileCursor::try_new(
                    request.project_id(),
                    request.parent_id(),
                    snapshot_version,
                    last.exact_name.as_str(),
                    last.id,
                )
                .map_err(|_| FileRepositoryError::Unavailable)?,
            )
        } else {
            None
        };
        transaction.commit().await.map_err(map_sql)?;
        Ok(FilePage {
            items,
            next_cursor,
            snapshot_version,
        })
    }
}

async fn validate_project(
    transaction: &mut Transaction<'_, Sqlite>,
    project_id: ProjectId,
) -> Result<(), FileRepositoryError> {
    let row = sqlx::query("SELECT status, deleted_at FROM project WHERE id = ?")
        .bind(project_id.to_string())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(map_sql)?
        .ok_or(FileRepositoryError::ProjectNotFound)?;
    let status: String = row.try_get("status").map_err(map_sql)?;
    let deleted_at: Option<String> = row.try_get("deleted_at").map_err(map_sql)?;
    if deleted_at.is_some() {
        return Err(FileRepositoryError::ProjectNotFound);
    }
    if !matches!(status.as_str(), "active" | "archived") {
        return Err(FileRepositoryError::Unavailable);
    }
    Ok(())
}

async fn directory_path(
    transaction: &mut Transaction<'_, Sqlite>,
    project_id: ProjectId,
    folder_id: FileEntryId,
) -> Result<String, FileRepositoryError> {
    let mut current = Some(folder_id);
    let mut names = Vec::new();
    let mut visited = HashSet::new();
    while let Some(id) = current {
        if names.len() >= MAX_DIRECTORY_DEPTH || !visited.insert(id) {
            return Err(FileRepositoryError::Unavailable);
        }
        let row = sqlx::query(
            "SELECT parent_id, exact_name, kind, state
             FROM file_entry WHERE id = ? AND project_id = ?",
        )
        .bind(id.to_string())
        .bind(project_id.to_string())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(map_sql)?
        .ok_or({
            if names.is_empty() {
                FileRepositoryError::FolderNotFound
            } else {
                FileRepositoryError::Unavailable
            }
        })?;
        let parent_id: Option<String> = row.try_get("parent_id").map_err(map_sql)?;
        let exact_name: String = row.try_get("exact_name").map_err(map_sql)?;
        let kind: String = row.try_get("kind").map_err(map_sql)?;
        let state: String = row.try_get("state").map_err(map_sql)?;
        let kind = kind
            .parse::<FileKind>()
            .map_err(|_| FileRepositoryError::Unavailable)?;
        let state = state
            .parse::<FileState>()
            .map_err(|_| FileRepositoryError::Unavailable)?;
        if kind != FileKind::Directory {
            return Err(if names.is_empty() {
                FileRepositoryError::InvalidFolder
            } else {
                FileRepositoryError::Unavailable
            });
        }
        if state == FileState::Unsupported {
            return Err(FileRepositoryError::UnsupportedFolder);
        }
        if !matches!(state, FileState::Live | FileState::Settling) {
            return Err(if names.is_empty() {
                FileRepositoryError::FolderNotFound
            } else {
                FileRepositoryError::Unavailable
            });
        }
        names.push(
            FileExactName::parse(exact_name)
                .map_err(|_| FileRepositoryError::Unavailable)?
                .as_str()
                .to_owned(),
        );
        current = parent_id.map(parse_canonical_file_id).transpose()?;
    }
    names.reverse();
    let path = names.join("/");
    if path.len() > MAX_RELATIVE_PATH_BYTES {
        return Err(FileRepositoryError::Unavailable);
    }
    Ok(path)
}

async fn fetch_rows(
    transaction: &mut Transaction<'_, Sqlite>,
    project_id: ProjectId,
    parent_id: Option<FileEntryId>,
    cursor: Option<&FileCursor>,
    limit: i64,
) -> Result<Vec<SqliteRow>, FileRepositoryError> {
    const COLUMNS: &str =
        "id, parent_id, exact_name, kind, platform_kind, volume_serial, filesystem_file_id,
         size, mtime_filetime_100ns, hash, hash_state, state, revision, scan_generation,
         observed_at";
    let project = project_id.to_string();
    let rows = match (parent_id, cursor) {
        (None, None) => {
            sqlx::query(&format!(
                "SELECT {COLUMNS} FROM file_entry
                 WHERE project_id = ? AND parent_id IS NULL
                   AND state IN ('live', 'settling', 'unsupported')
                 ORDER BY exact_name COLLATE WINDOWS_ORDINAL_CI_V1,
                          id COLLATE BINARY
                 LIMIT ?"
            ))
            .bind(project)
            .bind(limit)
            .fetch_all(&mut **transaction)
            .await
        }
        (None, Some(cursor)) => {
            sqlx::query(&format!(
                "SELECT {COLUMNS} FROM file_entry
                 WHERE project_id = ? AND parent_id IS NULL
                   AND state IN ('live', 'settling', 'unsupported')
                   AND (
                     exact_name COLLATE WINDOWS_ORDINAL_CI_V1 >
                       ? COLLATE WINDOWS_ORDINAL_CI_V1
                     OR (
                       exact_name COLLATE WINDOWS_ORDINAL_CI_V1 =
                         ? COLLATE WINDOWS_ORDINAL_CI_V1
                       AND id COLLATE BINARY > ? COLLATE BINARY
                     )
                   )
                 ORDER BY exact_name COLLATE WINDOWS_ORDINAL_CI_V1,
                          id COLLATE BINARY
                 LIMIT ?"
            ))
            .bind(project)
            .bind(cursor.exact_name())
            .bind(cursor.exact_name())
            .bind(cursor.entry_id().to_string())
            .bind(limit)
            .fetch_all(&mut **transaction)
            .await
        }
        (Some(parent_id), None) => {
            sqlx::query(&format!(
                "SELECT {COLUMNS} FROM file_entry
                 WHERE project_id = ? AND parent_id = ?
                   AND state IN ('live', 'settling', 'unsupported')
                 ORDER BY exact_name COLLATE WINDOWS_ORDINAL_CI_V1,
                          id COLLATE BINARY
                 LIMIT ?"
            ))
            .bind(project)
            .bind(parent_id.to_string())
            .bind(limit)
            .fetch_all(&mut **transaction)
            .await
        }
        (Some(parent_id), Some(cursor)) => {
            sqlx::query(&format!(
                "SELECT {COLUMNS} FROM file_entry
                 WHERE project_id = ? AND parent_id = ?
                   AND state IN ('live', 'settling', 'unsupported')
                   AND (
                     exact_name COLLATE WINDOWS_ORDINAL_CI_V1 >
                       ? COLLATE WINDOWS_ORDINAL_CI_V1
                     OR (
                       exact_name COLLATE WINDOWS_ORDINAL_CI_V1 =
                         ? COLLATE WINDOWS_ORDINAL_CI_V1
                       AND id COLLATE BINARY > ? COLLATE BINARY
                     )
                   )
                 ORDER BY exact_name COLLATE WINDOWS_ORDINAL_CI_V1,
                          id COLLATE BINARY
                 LIMIT ?"
            ))
            .bind(project)
            .bind(parent_id.to_string())
            .bind(cursor.exact_name())
            .bind(cursor.exact_name())
            .bind(cursor.entry_id().to_string())
            .bind(limit)
            .fetch_all(&mut **transaction)
            .await
        }
    };
    rows.map_err(map_sql)
}

fn row_to_entry(
    row: SqliteRow,
    project_id: ProjectId,
    expected_parent: Option<FileEntryId>,
    parent_path: &str,
) -> Result<FileEntry, FileRepositoryError> {
    let id = parse_canonical_file_id(row.try_get::<String, _>("id").map_err(map_sql)?)?;
    let parent_id = row
        .try_get::<Option<String>, _>("parent_id")
        .map_err(map_sql)?
        .map(parse_canonical_file_id)
        .transpose()?;
    if parent_id != expected_parent {
        return Err(FileRepositoryError::Unavailable);
    }
    let exact_name = FileExactName::parse(row.try_get::<String, _>("exact_name").map_err(map_sql)?)
        .map_err(|_| FileRepositoryError::Unavailable)?;
    let relative_path = if parent_path.is_empty() {
        exact_name.as_str().to_owned()
    } else {
        format!("{parent_path}/{}", exact_name.as_str())
    };
    if relative_path.len() > MAX_RELATIVE_PATH_BYTES {
        return Err(FileRepositoryError::Unavailable);
    }
    let kind = row
        .try_get::<String, _>("kind")
        .map_err(map_sql)?
        .parse()
        .map_err(|_| FileRepositoryError::Unavailable)?;
    let state: FileState = row
        .try_get::<String, _>("state")
        .map_err(map_sql)?
        .parse()
        .map_err(|_| FileRepositoryError::Unavailable)?;
    if !state.is_listable() {
        return Err(FileRepositoryError::Unavailable);
    }
    let platform_identity = PlatformIdentity::try_new(
        row.try_get::<String, _>("platform_kind").map_err(map_sql)?,
        row.try_get::<Option<Vec<u8>>, _>("volume_serial")
            .map_err(map_sql)?,
        row.try_get::<Option<Vec<u8>>, _>("filesystem_file_id")
            .map_err(map_sql)?,
    )
    .map_err(|_| FileRepositoryError::Unavailable)?;
    let size: i64 = row.try_get("size").map_err(map_sql)?;
    let mtime_filetime_100ns: i64 = row.try_get("mtime_filetime_100ns").map_err(map_sql)?;
    let revision: i64 = row.try_get("revision").map_err(map_sql)?;
    let scan_generation: i64 = row.try_get("scan_generation").map_err(map_sql)?;
    if size < 0 || mtime_filetime_100ns < 0 || revision < 1 || scan_generation < 0 {
        return Err(FileRepositoryError::Unavailable);
    }
    let hash = row
        .try_get::<Option<Vec<u8>>, _>("hash")
        .map_err(map_sql)?
        .map(|value| {
            value
                .try_into()
                .map_err(|_| FileRepositoryError::Unavailable)
        })
        .transpose()?;
    let hash_state = row
        .try_get::<String, _>("hash_state")
        .map_err(map_sql)?
        .parse::<FileHashState>()
        .map_err(|_| FileRepositoryError::Unavailable)?;
    if (hash_state == FileHashState::Ready) != hash.is_some() {
        return Err(FileRepositoryError::Unavailable);
    }
    let observed_at = OffsetDateTime::parse(
        &row.try_get::<String, _>("observed_at").map_err(map_sql)?,
        &Rfc3339,
    )
    .map_err(|_| FileRepositoryError::Unavailable)?;
    Ok(FileEntry {
        id,
        project_id,
        parent_id,
        exact_name,
        relative_path,
        kind,
        platform_identity,
        size,
        mtime_filetime_100ns,
        hash,
        hash_state,
        state,
        revision,
        scan_generation,
        observed_at,
    })
}

fn parse_canonical_file_id(value: String) -> Result<FileEntryId, FileRepositoryError> {
    let parsed = value
        .parse::<FileEntryId>()
        .map_err(|_| FileRepositoryError::Unavailable)?;
    if parsed.to_string() != value {
        return Err(FileRepositoryError::Unavailable);
    }
    Ok(parsed)
}

fn map_sql(_: sqlx::Error) -> FileRepositoryError {
    FileRepositoryError::Unavailable
}

//! Database access.

use std::{path::Path, time::Duration};

use sqlx::{
    Row, SqlitePool,
    migrate::MigrateError,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use uuid::Uuid;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Maximum number of Unicode scalar values retained for an internal upload failure reason.
pub const MAX_FAILURE_REASON_CHARS: usize = 500;

#[derive(Debug)]
pub enum DbError {
    Sql(sqlx::Error),
    Migration(MigrateError),
    Conflict,
    ValueOutOfRange,
    InvalidTransition,
    NotFound,
    InvalidFailureReason,
    InvalidTimestamp,
    CorruptData,
}

impl std::fmt::Display for DbError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Sql(_) | Self::Migration(_) => "database operation failed",
            Self::Conflict => "database constraint conflict",
            Self::ValueOutOfRange => "numeric value is out of range",
            Self::InvalidTransition => "upload state transition is invalid",
            Self::NotFound => "database record was not found",
            Self::InvalidFailureReason => "upload failure reason is invalid",
            Self::InvalidTimestamp => "timestamp must be canonical RFC3339 UTC",
            Self::CorruptData => "database contains invalid data",
        })
    }
}

impl std::error::Error for DbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sql(source) => Some(source),
            Self::Migration(source) => Some(source),
            Self::Conflict
            | Self::ValueOutOfRange
            | Self::InvalidTransition
            | Self::NotFound
            | Self::InvalidFailureReason
            | Self::InvalidTimestamp
            | Self::CorruptData => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    id: Uuid,
    name: String,
    created_at: String,
}

impl ProjectRow {
    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn created_at(&self) -> &str {
        &self.created_at
    }
}

#[derive(Debug, Clone)]
pub struct NewProject {
    id: Uuid,
    name: String,
    created_at: String,
}

impl NewProject {
    pub fn new(id: Uuid, name: impl Into<String>) -> Result<Self, DbError> {
        Ok(Self {
            id,
            name: name.into(),
            created_at: current_timestamp()?,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_created_at(
        id: Uuid,
        name: impl Into<String>,
        created_at: impl Into<String>,
    ) -> Result<Self, DbError> {
        let created_at = created_at.into();
        validate_timestamp(&created_at)?;
        Ok(Self {
            id,
            name: name.into(),
            created_at,
        })
    }
}

fn validate_timestamp(value: &str) -> Result<(), DbError> {
    parse_canonical_timestamp(value).map(|_| ())
}

fn parse_canonical_timestamp(value: &str) -> Result<OffsetDateTime, DbError> {
    let parsed = OffsetDateTime::parse(value, &Rfc3339).map_err(|_| DbError::InvalidTimestamp)?;
    if parsed.offset() != UtcOffset::UTC
        || parsed
            .format(&Rfc3339)
            .map_err(|_| DbError::InvalidTimestamp)?
            != value
    {
        return Err(DbError::InvalidTimestamp);
    }
    Ok(parsed)
}

fn to_sqlite_integer(value: u64) -> Result<i64, DbError> {
    i64::try_from(value).map_err(|_| DbError::ValueOutOfRange)
}

fn is_valid_failure_reason(reason: &str) -> bool {
    reason.chars().count() <= MAX_FAILURE_REASON_CHARS && !reason.chars().any(char::is_control)
}

fn current_timestamp() -> Result<String, DbError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| DbError::InvalidTimestamp)
}

fn stored_timestamp_key(value: &str) -> Result<i128, DbError> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map(OffsetDateTime::unix_timestamp_nanos)
        .map_err(|_| DbError::CorruptData)
}

fn decode_project(row: sqlx::sqlite::SqliteRow) -> Result<ProjectRow, DbError> {
    let stored_id = row
        .try_get::<String, _>("id")
        .map_err(|_| DbError::CorruptData)?;
    let id = Uuid::parse_str(&stored_id).map_err(|_| DbError::CorruptData)?;
    let name: String = row.try_get("name").map_err(|_| DbError::CorruptData)?;
    let created_at: String = row
        .try_get("created_at")
        .map_err(|_| DbError::CorruptData)?;
    validate_timestamp(&created_at).map_err(|_| DbError::CorruptData)?;
    Ok(ProjectRow {
        id,
        name,
        created_at,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadState {
    Active,
    Finalizing,
    Complete,
    Failed,
}

impl UploadState {
    fn as_db_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Finalizing => "finalizing",
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }

    fn from_db_str(value: &str) -> Result<Self, DbError> {
        match value {
            "active" => Ok(Self::Active),
            "finalizing" => Ok(Self::Finalizing),
            "complete" => Ok(Self::Complete),
            "failed" => Ok(Self::Failed),
            _ => Err(DbError::CorruptData),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadRow {
    id: Uuid,
    project_id: Uuid,
    file_name: String,
    total_size: u64,
    committed_offset: u64,
    state: UploadState,
    failure_reason: Option<String>,
    created_at: String,
    updated_at: String,
}

impl UploadRow {
    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn project_id(&self) -> Uuid {
        self.project_id
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    pub fn committed_offset(&self) -> u64 {
        self.committed_offset
    }

    pub fn state(&self) -> UploadState {
        self.state
    }

    pub fn failure_reason(&self) -> Option<&str> {
        self.failure_reason.as_deref()
    }

    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    pub fn updated_at(&self) -> &str {
        &self.updated_at
    }
}

#[derive(Debug, Clone)]
pub struct NewUpload {
    id: Uuid,
    project_id: Uuid,
    file_name: String,
    total_size: u64,
    created_at: String,
    updated_at: String,
}

impl NewUpload {
    pub fn new(
        id: Uuid,
        project_id: Uuid,
        file_name: impl Into<String>,
        total_size: u64,
    ) -> Result<Self, DbError> {
        let timestamp = current_timestamp()?;
        Ok(Self {
            id,
            project_id,
            file_name: file_name.into(),
            total_size,
            created_at: timestamp.clone(),
            updated_at: timestamp,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_timestamps(
        id: Uuid,
        project_id: Uuid,
        file_name: impl Into<String>,
        total_size: u64,
        created_at: impl Into<String>,
        updated_at: impl Into<String>,
    ) -> Result<Self, DbError> {
        let created_at = created_at.into();
        let updated_at = updated_at.into();
        let created_time = parse_canonical_timestamp(&created_at)?;
        let updated_time = parse_canonical_timestamp(&updated_at)?;
        if updated_time < created_time {
            return Err(DbError::InvalidTimestamp);
        }
        Ok(Self {
            id,
            project_id,
            file_name: file_name.into(),
            total_size,
            created_at,
            updated_at,
        })
    }
}

fn decode_upload(row: sqlx::sqlite::SqliteRow) -> Result<UploadRow, DbError> {
    let stored_id = row
        .try_get::<String, _>("id")
        .map_err(|_| DbError::CorruptData)?;
    let id = Uuid::parse_str(&stored_id).map_err(|_| DbError::CorruptData)?;
    let stored_project_id = row
        .try_get::<String, _>("project_id")
        .map_err(|_| DbError::CorruptData)?;
    let project_id = Uuid::parse_str(&stored_project_id).map_err(|_| DbError::CorruptData)?;
    let stored_total_size = row
        .try_get::<i64, _>("total_size")
        .map_err(|_| DbError::CorruptData)?;
    let total_size = u64::try_from(stored_total_size).map_err(|_| DbError::CorruptData)?;
    let stored_committed_offset = row
        .try_get::<i64, _>("committed_offset")
        .map_err(|_| DbError::CorruptData)?;
    let committed_offset =
        u64::try_from(stored_committed_offset).map_err(|_| DbError::CorruptData)?;
    if committed_offset > total_size {
        return Err(DbError::CorruptData);
    }
    let file_name = row.try_get("file_name").map_err(|_| DbError::CorruptData)?;
    let stored_state = row
        .try_get::<String, _>("state")
        .map_err(|_| DbError::CorruptData)?;
    let state = UploadState::from_db_str(&stored_state)?;
    let failure_reason: Option<String> = row
        .try_get("failure_reason")
        .map_err(|_| DbError::CorruptData)?;
    if failure_reason
        .as_deref()
        .is_some_and(|reason| !is_valid_failure_reason(reason))
    {
        return Err(DbError::CorruptData);
    }
    let created_at: String = row
        .try_get("created_at")
        .map_err(|_| DbError::CorruptData)?;
    let updated_at: String = row
        .try_get("updated_at")
        .map_err(|_| DbError::CorruptData)?;
    validate_timestamp(&created_at).map_err(|_| DbError::CorruptData)?;
    validate_timestamp(&updated_at).map_err(|_| DbError::CorruptData)?;
    Ok(UploadRow {
        id,
        project_id,
        file_name,
        total_size,
        committed_offset,
        state,
        failure_reason,
        created_at,
        updated_at,
    })
}

impl From<sqlx::Error> for DbError {
    fn from(source: sqlx::Error) -> Self {
        Self::Sql(source)
    }
}

impl From<MigrateError> for DbError {
    fn from(source: MigrateError) -> Self {
        Self::Migration(source)
    }
}

#[derive(Debug, Clone)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    async fn upload_transition_error(&self, id: Uuid) -> DbError {
        match self.get_upload(id).await {
            Ok(None) => DbError::NotFound,
            Ok(Some(_)) => DbError::InvalidTransition,
            Err(error) => error,
        }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5))
            .synchronous(SqliteSynchronous::Full);
        let pool = SqlitePoolOptions::new().connect_with(options).await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn close(self) {
        self.pool.close().await;
    }

    pub async fn create_project(&self, project: NewProject) -> Result<ProjectRow, DbError> {
        sqlx::query("INSERT INTO project (id, name, created_at) VALUES (?, ?, ?)")
            .bind(project.id.to_string())
            .bind(&project.name)
            .bind(&project.created_at)
            .execute(&self.pool)
            .await
            .map_err(|error| {
                if error
                    .as_database_error()
                    .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
                {
                    DbError::Conflict
                } else {
                    DbError::Sql(error)
                }
            })?;
        Ok(ProjectRow {
            id: project.id,
            name: project.name,
            created_at: project.created_at,
        })
    }

    pub async fn get_project(&self, id: Uuid) -> Result<Option<ProjectRow>, DbError> {
        sqlx::query("SELECT id, name, created_at FROM project WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .map(decode_project)
            .transpose()
    }

    pub async fn list_projects(&self) -> Result<Vec<ProjectRow>, DbError> {
        let rows = sqlx::query("SELECT id, name, created_at FROM project")
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(decode_project)
            .collect::<Result<Vec<_>, _>>()?;
        let mut ordered = rows
            .into_iter()
            .map(|row| Ok((stored_timestamp_key(&row.created_at)?, row.id, row)))
            .collect::<Result<Vec<_>, DbError>>()?;
        ordered.sort_by_key(|(created_at, id, _)| (*created_at, *id));
        Ok(ordered.into_iter().map(|(_, _, row)| row).collect())
    }

    pub async fn delete_project(&self, id: Uuid) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM project WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn create_upload(&self, upload: NewUpload) -> Result<UploadRow, DbError> {
        let total_size = to_sqlite_integer(upload.total_size)?;
        sqlx::query(
            "INSERT INTO upload_session \
             (id, project_id, file_name, total_size, committed_offset, state, failure_reason, created_at, updated_at) \
             VALUES (?, ?, ?, ?, 0, ?, NULL, ?, ?)",
        )
        .bind(upload.id.to_string())
        .bind(upload.project_id.to_string())
        .bind(&upload.file_name)
        .bind(total_size)
        .bind(UploadState::Active.as_db_str())
        .bind(&upload.created_at)
        .bind(&upload.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|error| match error.as_database_error() {
            Some(database_error) if database_error.is_unique_violation() => DbError::Conflict,
            Some(database_error) if database_error.is_foreign_key_violation() => DbError::NotFound,
            _ => DbError::Sql(error),
        })?;
        Ok(UploadRow {
            id: upload.id,
            project_id: upload.project_id,
            file_name: upload.file_name,
            total_size: upload.total_size,
            committed_offset: 0,
            state: UploadState::Active,
            failure_reason: None,
            created_at: upload.created_at,
            updated_at: upload.updated_at,
        })
    }

    pub async fn get_upload(&self, id: Uuid) -> Result<Option<UploadRow>, DbError> {
        sqlx::query(
            "SELECT id, project_id, file_name, total_size, committed_offset, state, \
                    failure_reason, created_at, updated_at \
             FROM upload_session WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .map(decode_upload)
        .transpose()
    }

    pub async fn advance_offset(
        &self,
        id: Uuid,
        expected: u64,
        next: u64,
    ) -> Result<bool, DbError> {
        let expected = to_sqlite_integer(expected)?;
        let next = to_sqlite_integer(next)?;
        let updated_at = current_timestamp()?;
        let result = sqlx::query(
            "UPDATE upload_session \
             SET committed_offset = ?, updated_at = ? \
             WHERE id = ? AND state = 'active' AND committed_offset = ? \
               AND ? >= ? AND ? <= total_size",
        )
        .bind(next)
        .bind(updated_at)
        .bind(id.to_string())
        .bind(expected)
        .bind(next)
        .bind(expected)
        .bind(next)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn mark_finalizing(&self, id: Uuid, expected: u64) -> Result<bool, DbError> {
        let expected = to_sqlite_integer(expected)?;
        let result = sqlx::query(
            "UPDATE upload_session SET state = 'finalizing', updated_at = ? \
             WHERE id = ? AND state = 'active' AND committed_offset = ? \
               AND committed_offset = total_size",
        )
        .bind(current_timestamp()?)
        .bind(id.to_string())
        .bind(expected)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn mark_complete(&self, id: Uuid) -> Result<(), DbError> {
        let result = sqlx::query(
            "UPDATE upload_session \
             SET state = 'complete', failure_reason = NULL, updated_at = ? \
             WHERE id = ? AND state = 'finalizing'",
        )
        .bind(current_timestamp()?)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(self.upload_transition_error(id).await)
        }
    }

    /// Marks an in-progress upload failed using a caller-supplied, already-redacted summary.
    pub async fn mark_failed(&self, id: Uuid, reason: &str) -> Result<(), DbError> {
        if !is_valid_failure_reason(reason) {
            return Err(DbError::InvalidFailureReason);
        }
        let result = sqlx::query(
            "UPDATE upload_session \
             SET state = 'failed', failure_reason = ?, updated_at = ? \
             WHERE id = ? AND state IN ('active', 'finalizing')",
        )
        .bind(reason)
        .bind(current_timestamp()?)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(self.upload_transition_error(id).await)
        }
    }

    pub async fn recoverable_uploads(&self) -> Result<Vec<UploadRow>, DbError> {
        let rows = sqlx::query(
            "SELECT id, project_id, file_name, total_size, committed_offset, state, \
                    failure_reason, created_at, updated_at \
             FROM upload_session \
             WHERE state IN ('active', 'finalizing')",
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(decode_upload)
        .collect::<Result<Vec<_>, _>>()?;
        let mut ordered = rows
            .into_iter()
            .map(|row| Ok((stored_timestamp_key(&row.created_at)?, row.id, row)))
            .collect::<Result<Vec<_>, DbError>>()?;
        ordered.sort_by_key(|(created_at, id, _)| (*created_at, *id));
        Ok(ordered.into_iter().map(|(_, _, row)| row).collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use sqlx::Row as _;
    use tempfile::TempDir;
    use uuid::Uuid;

    use super::{Database, DbError, MAX_FAILURE_REASON_CHARS, NewProject, NewUpload, UploadState};

    async fn test_database() -> (TempDir, Database) {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(directory.path().join("cellar.sqlite"))
            .await
            .unwrap();
        (directory, database)
    }

    #[tokio::test]
    async fn migration_creates_only_expected_domain_tables_and_indexes() {
        let (directory, database) = test_database().await;
        let objects = sqlx::query_as::<_, (String, String)>(
            "SELECT type, name FROM sqlite_master \
             WHERE (type = 'table' OR type = 'index') \
               AND name NOT LIKE 'sqlite_%' \
               AND name != '_sqlx_migrations'",
        )
        .fetch_all(&database.pool)
        .await
        .unwrap()
        .into_iter()
        .collect::<BTreeSet<_>>();

        assert_eq!(
            objects,
            BTreeSet::from([
                ("index".to_owned(), "upload_session_project_idx".to_owned()),
                ("index".to_owned(), "upload_session_state_idx".to_owned()),
                ("table".to_owned(), "project".to_owned()),
                ("table".to_owned(), "upload_session".to_owned()),
            ])
        );

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn migration_has_exact_columns_foreign_key_and_index_columns() {
        let (directory, database) = test_database().await;

        let project_columns = sqlx::query("PRAGMA table_info(project)")
            .fetch_all(&database.pool)
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                (
                    row.get::<i64, _>("cid"),
                    row.get::<String, _>("name"),
                    row.get::<String, _>("type"),
                    row.get::<i64, _>("notnull"),
                    row.get::<Option<String>, _>("dflt_value"),
                    row.get::<i64, _>("pk"),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            project_columns,
            vec![
                (0, "id".to_owned(), "TEXT".to_owned(), 0, None, 1),
                (1, "name".to_owned(), "TEXT".to_owned(), 1, None, 0),
                (2, "created_at".to_owned(), "TEXT".to_owned(), 1, None, 0,),
            ]
        );

        let upload_columns = sqlx::query("PRAGMA table_info(upload_session)")
            .fetch_all(&database.pool)
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                (
                    row.get::<i64, _>("cid"),
                    row.get::<String, _>("name"),
                    row.get::<String, _>("type"),
                    row.get::<i64, _>("notnull"),
                    row.get::<Option<String>, _>("dflt_value"),
                    row.get::<i64, _>("pk"),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            upload_columns,
            vec![
                (0, "id".to_owned(), "TEXT".to_owned(), 0, None, 1),
                (1, "project_id".to_owned(), "TEXT".to_owned(), 1, None, 0,),
                (2, "file_name".to_owned(), "TEXT".to_owned(), 1, None, 0,),
                (3, "total_size".to_owned(), "INTEGER".to_owned(), 1, None, 0,),
                (
                    4,
                    "committed_offset".to_owned(),
                    "INTEGER".to_owned(),
                    1,
                    Some("0".to_owned()),
                    0,
                ),
                (5, "state".to_owned(), "TEXT".to_owned(), 1, None, 0),
                (
                    6,
                    "failure_reason".to_owned(),
                    "TEXT".to_owned(),
                    0,
                    None,
                    0,
                ),
                (7, "created_at".to_owned(), "TEXT".to_owned(), 1, None, 0,),
                (8, "updated_at".to_owned(), "TEXT".to_owned(), 1, None, 0,),
            ]
        );

        let foreign_keys = sqlx::query("PRAGMA foreign_key_list(upload_session)")
            .fetch_all(&database.pool)
            .await
            .unwrap();
        assert_eq!(foreign_keys.len(), 1);
        let foreign_key = &foreign_keys[0];
        assert_eq!(foreign_key.get::<String, _>("table"), "project");
        assert_eq!(foreign_key.get::<String, _>("from"), "project_id");
        assert_eq!(foreign_key.get::<String, _>("to"), "id");
        assert_eq!(foreign_key.get::<String, _>("on_delete"), "CASCADE");

        for (index, expected_column) in [
            ("upload_session_project_idx", "project_id"),
            ("upload_session_state_idx", "state"),
        ] {
            let columns = sqlx::query(&format!("PRAGMA index_info('{index}')"))
                .fetch_all(&database.pool)
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.get::<String, _>("name"))
                .collect::<Vec<_>>();
            assert_eq!(columns, vec![expected_column.to_owned()]);
        }

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn migration_enforces_upload_check_constraints() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000200");
        database
            .create_project(
                NewProject::with_created_at(project_id, "Checks", "2026-08-04T10:00:00Z").unwrap(),
            )
            .await
            .unwrap();

        for (upload_id, total_size, committed_offset, state) in [
            (
                "0198f67e-9c0b-7000-8000-000000000201",
                -1_i64,
                0_i64,
                "active",
            ),
            (
                "0198f67e-9c0b-7000-8000-000000000202",
                1_i64,
                -1_i64,
                "active",
            ),
            (
                "0198f67e-9c0b-7000-8000-000000000203",
                1_i64,
                0_i64,
                "unknown",
            ),
        ] {
            let error = sqlx::query(
                "INSERT INTO upload_session \
                 (id, project_id, file_name, total_size, committed_offset, state, \
                  failure_reason, created_at, updated_at) \
                 VALUES (?, ?, 'invalid.bin', ?, ?, ?, NULL, ?, ?)",
            )
            .bind(upload_id)
            .bind(project_id.to_string())
            .bind(total_size)
            .bind(committed_offset)
            .bind(state)
            .bind("2026-08-04T10:00:01Z")
            .bind("2026-08-04T10:00:01Z")
            .execute(&database.pool)
            .await
            .unwrap_err();
            assert!(
                error
                    .as_database_error()
                    .is_some_and(sqlx::error::DatabaseError::is_check_violation),
                "expected a CHECK violation for total_size={total_size}, \
                 committed_offset={committed_offset}, state={state}"
            );
        }

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn connections_use_required_sqlite_pragmas() {
        let (directory, database) = test_database().await;
        let mut connection = database.pool.acquire().await.unwrap();

        let foreign_keys = sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        let journal_mode = sqlx::query_scalar::<_, String>("PRAGMA journal_mode")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        let synchronous = sqlx::query_scalar::<_, i64>("PRAGMA synchronous")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        let busy_timeout = sqlx::query_scalar::<_, i64>("PRAGMA busy_timeout")
            .fetch_one(&mut *connection)
            .await
            .unwrap();

        assert_eq!(foreign_keys, 1);
        assert_eq!(journal_mode, "wal");
        assert_eq!(synchronous, 2);
        assert_eq!(busy_timeout, 5_000);

        drop(connection);
        database.close().await;
        drop(directory);
    }

    fn id(value: &str) -> Uuid {
        Uuid::parse_str(value).unwrap()
    }

    #[test]
    fn constructors_generate_canonical_utc_timestamps() {
        let project_id = id("0198f67e-9c0b-7000-8000-000000000000");
        let project = NewProject::new(project_id, "Generated").unwrap();
        super::validate_timestamp(&project.created_at).unwrap();

        let upload = NewUpload::new(
            id("0198f67e-9c0b-7000-8000-000000000001"),
            project_id,
            "generated.bin",
            1,
        )
        .unwrap();
        super::validate_timestamp(&upload.created_at).unwrap();
        assert_eq!(upload.created_at, upload.updated_at);
    }

    #[test]
    fn upload_constructor_rejects_updated_at_before_created_at() {
        let error = NewUpload::with_timestamps(
            id("0198f67e-9c0b-7000-8000-000000000006"),
            id("0198f67e-9c0b-7000-8000-000000000007"),
            "reversed.bin",
            1,
            "2026-08-04T10:00:01Z",
            "2026-08-04T10:00:00Z",
        )
        .unwrap_err();
        assert!(matches!(error, DbError::InvalidTimestamp));
    }

    #[tokio::test]
    async fn projects_round_trip_and_list_in_deterministic_order() {
        let (directory, database) = test_database().await;
        let later = NewProject::with_created_at(
            id("0198f67e-9c0b-7000-8000-000000000003"),
            "Later",
            "2026-08-04T10:00:01Z",
        )
        .unwrap();
        let first_tie = NewProject::with_created_at(
            id("0198f67e-9c0b-7000-8000-000000000001"),
            "First tie",
            "2026-08-04T10:00:00Z",
        )
        .unwrap();
        let second_tie = NewProject::with_created_at(
            id("0198f67e-9c0b-7000-8000-000000000002"),
            "Second tie",
            "2026-08-04T10:00:00Z",
        )
        .unwrap();

        let created = database.create_project(later).await.unwrap();
        assert_eq!(created.name(), "Later");
        assert_eq!(created.created_at(), "2026-08-04T10:00:01Z");
        database.create_project(second_tie).await.unwrap();
        database.create_project(first_tie).await.unwrap();

        let fetched = database.get_project(created.id()).await.unwrap().unwrap();
        assert_eq!(fetched, created);
        let listed = database.list_projects().await.unwrap();
        assert_eq!(
            listed.iter().map(|row| row.id()).collect::<Vec<_>>(),
            vec![
                id("0198f67e-9c0b-7000-8000-000000000001"),
                id("0198f67e-9c0b-7000-8000-000000000002"),
                id("0198f67e-9c0b-7000-8000-000000000003"),
            ]
        );

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn project_order_is_chronological_across_fractional_seconds() {
        let (directory, database) = test_database().await;
        let whole_second = id("0198f67e-9c0b-7000-8000-000000000004");
        let fractional = id("0198f67e-9c0b-7000-8000-000000000005");
        database
            .create_project(
                NewProject::with_created_at(fractional, "Fractional", "2026-08-04T10:00:00.1Z")
                    .unwrap(),
            )
            .await
            .unwrap();
        database
            .create_project(
                NewProject::with_created_at(whole_second, "Whole", "2026-08-04T10:00:00Z").unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            database
                .list_projects()
                .await
                .unwrap()
                .iter()
                .map(|row| row.id())
                .collect::<Vec<_>>(),
            vec![whole_second, fractional]
        );

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn duplicate_project_id_returns_a_safe_conflict_error() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000010");
        database
            .create_project(
                NewProject::with_created_at(project_id, "Original", "2026-08-04T10:00:00Z")
                    .unwrap(),
            )
            .await
            .unwrap();

        let error = database
            .create_project(
                NewProject::with_created_at(project_id, "Duplicate", "2026-08-04T10:00:01Z")
                    .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, DbError::Conflict));
        let display = error.to_string();
        assert_eq!(display, "database constraint conflict");
        assert!(!display.contains(project_id.to_string().as_str()));
        assert!(!display.contains("INSERT"));

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn deleting_a_project_reports_existence_and_cascades_uploads() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000020");
        database
            .create_project(
                NewProject::with_created_at(project_id, "Cascade", "2026-08-04T10:00:00Z").unwrap(),
            )
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO upload_session \
             (id, project_id, file_name, total_size, state, created_at, updated_at) \
             VALUES (?, ?, 'archive.bin', 10, 'active', ?, ?)",
        )
        .bind("0198f67e-9c0b-7000-8000-000000000021")
        .bind(project_id.to_string())
        .bind("2026-08-04T10:00:00Z")
        .bind("2026-08-04T10:00:00Z")
        .execute(&database.pool)
        .await
        .unwrap();

        assert!(database.delete_project(project_id).await.unwrap());
        assert!(!database.delete_project(project_id).await.unwrap());
        let upload_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM upload_session")
            .fetch_one(&database.pool)
            .await
            .unwrap();
        assert_eq!(upload_count, 0);

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn uploads_round_trip_with_typed_active_state() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000030");
        database
            .create_project(
                NewProject::with_created_at(project_id, "Uploads", "2026-08-04T10:00:00Z").unwrap(),
            )
            .await
            .unwrap();
        let upload_id = id("0198f67e-9c0b-7000-8000-000000000031");
        let upload = NewUpload::with_timestamps(
            upload_id,
            project_id,
            "archive.bin",
            64,
            "2026-08-04T10:00:01Z",
            "2026-08-04T10:00:02Z",
        )
        .unwrap();

        let created = database.create_upload(upload).await.unwrap();
        assert_eq!(created.id(), upload_id);
        assert_eq!(created.project_id(), project_id);
        assert_eq!(created.file_name(), "archive.bin");
        assert_eq!(created.total_size(), 64);
        assert_eq!(created.committed_offset(), 0);
        assert_eq!(created.state(), UploadState::Active);
        assert_eq!(created.failure_reason(), None);
        assert_eq!(created.created_at(), "2026-08-04T10:00:01Z");
        assert_eq!(created.updated_at(), "2026-08-04T10:00:02Z");
        assert_eq!(database.get_upload(upload_id).await.unwrap(), Some(created));

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn upload_total_size_accepts_i64_max_and_rejects_larger_before_sql() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000040");
        database
            .create_project(
                NewProject::with_created_at(project_id, "Boundaries", "2026-08-04T10:00:00Z")
                    .unwrap(),
            )
            .await
            .unwrap();

        let maximum = NewUpload::with_timestamps(
            id("0198f67e-9c0b-7000-8000-000000000041"),
            project_id,
            "maximum.bin",
            i64::MAX as u64,
            "2026-08-04T10:00:01Z",
            "2026-08-04T10:00:01Z",
        )
        .unwrap();
        assert_eq!(
            database.create_upload(maximum).await.unwrap().total_size(),
            i64::MAX as u64
        );

        let too_large = NewUpload::with_timestamps(
            id("0198f67e-9c0b-7000-8000-000000000042"),
            id("0198f67e-9c0b-7000-8000-000000000099"),
            "too-large.bin",
            (i64::MAX as u64) + 1,
            "2026-08-04T10:00:02Z",
            "2026-08-04T10:00:02Z",
        )
        .unwrap();
        assert!(matches!(
            database.create_upload(too_large).await.unwrap_err(),
            DbError::ValueOutOfRange
        ));

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn advance_offset_is_atomic_monotonic_and_bounded() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000050");
        database
            .create_project(
                NewProject::with_created_at(project_id, "Offsets", "2026-08-04T10:00:00Z").unwrap(),
            )
            .await
            .unwrap();
        let upload_id = id("0198f67e-9c0b-7000-8000-000000000051");
        database
            .create_upload(
                NewUpload::with_timestamps(
                    upload_id,
                    project_id,
                    "offset.bin",
                    64,
                    "2026-08-04T10:00:01Z",
                    "2026-08-04T10:00:01Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();

        assert!(database.advance_offset(upload_id, 0, 32).await.unwrap());
        let after_success = database.get_upload(upload_id).await.unwrap().unwrap();
        assert_eq!(after_success.committed_offset(), 32);
        assert_ne!(after_success.updated_at(), "2026-08-04T10:00:01Z");

        assert!(!database.advance_offset(upload_id, 0, 32).await.unwrap());
        assert!(!database.advance_offset(upload_id, 32, 31).await.unwrap());
        assert!(!database.advance_offset(upload_id, 40, 48).await.unwrap());
        assert!(!database.advance_offset(upload_id, 32, 65).await.unwrap());
        let after_failures = database.get_upload(upload_id).await.unwrap().unwrap();
        assert_eq!(after_failures.committed_offset(), 32);
        assert_eq!(after_failures.updated_at(), after_success.updated_at());

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn advance_offset_rejects_values_above_i64_max_before_sql() {
        let (directory, database) = test_database().await;
        let missing_id = id("0198f67e-9c0b-7000-8000-000000000059");
        let too_large = (i64::MAX as u64) + 1;

        assert!(matches!(
            database.advance_offset(missing_id, too_large, 0).await,
            Err(DbError::ValueOutOfRange)
        ));
        assert!(matches!(
            database.advance_offset(missing_id, 0, too_large).await,
            Err(DbError::ValueOutOfRange)
        ));

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn upload_transitions_from_active_to_finalizing_to_complete() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000060");
        database
            .create_project(
                NewProject::with_created_at(project_id, "Transitions", "2026-08-04T10:00:00Z")
                    .unwrap(),
            )
            .await
            .unwrap();
        let upload_id = id("0198f67e-9c0b-7000-8000-000000000061");
        database
            .create_upload(
                NewUpload::with_timestamps(
                    upload_id,
                    project_id,
                    "complete.bin",
                    32,
                    "2026-08-04T10:00:01Z",
                    "2026-08-04T10:00:01Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(database.advance_offset(upload_id, 0, 32).await.unwrap());

        assert!(database.mark_finalizing(upload_id, 32).await.unwrap());
        assert_eq!(
            database
                .get_upload(upload_id)
                .await
                .unwrap()
                .unwrap()
                .state(),
            UploadState::Finalizing
        );
        database.mark_complete(upload_id).await.unwrap();
        let complete = database.get_upload(upload_id).await.unwrap().unwrap();
        assert_eq!(complete.state(), UploadState::Complete);
        assert_eq!(complete.failure_reason(), None);

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn finalizing_is_rejected_until_the_full_size_is_committed() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000070");
        database
            .create_project(
                NewProject::with_created_at(project_id, "Incomplete", "2026-08-04T10:00:00Z")
                    .unwrap(),
            )
            .await
            .unwrap();
        let upload_id = id("0198f67e-9c0b-7000-8000-000000000071");
        database
            .create_upload(
                NewUpload::with_timestamps(
                    upload_id,
                    project_id,
                    "incomplete.bin",
                    32,
                    "2026-08-04T10:00:01Z",
                    "2026-08-04T10:00:01Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();

        assert!(!database.mark_finalizing(upload_id, 0).await.unwrap());
        assert_eq!(
            database
                .get_upload(upload_id)
                .await
                .unwrap()
                .unwrap()
                .state(),
            UploadState::Active
        );

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn complete_distinguishes_invalid_transition_from_missing_upload() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000080");
        database
            .create_project(
                NewProject::with_created_at(
                    project_id,
                    "Invalid transitions",
                    "2026-08-04T10:00:00Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let upload_id = id("0198f67e-9c0b-7000-8000-000000000081");
        database
            .create_upload(
                NewUpload::with_timestamps(
                    upload_id,
                    project_id,
                    "active.bin",
                    0,
                    "2026-08-04T10:00:01Z",
                    "2026-08-04T10:00:01Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();

        assert!(matches!(
            database.mark_complete(upload_id).await,
            Err(DbError::InvalidTransition)
        ));
        assert!(matches!(
            database
                .mark_complete(id("0198f67e-9c0b-7000-8000-000000000089"))
                .await,
            Err(DbError::NotFound)
        ));

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn active_upload_can_transition_to_failed_with_a_reason() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000090");
        database
            .create_project(
                NewProject::with_created_at(project_id, "Failure", "2026-08-04T10:00:00Z").unwrap(),
            )
            .await
            .unwrap();
        let upload_id = id("0198f67e-9c0b-7000-8000-000000000091");
        database
            .create_upload(
                NewUpload::with_timestamps(
                    upload_id,
                    project_id,
                    "failed.bin",
                    32,
                    "2026-08-04T10:00:01Z",
                    "2026-08-04T10:00:01Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();

        database
            .mark_failed(upload_id, "checksum mismatch")
            .await
            .unwrap();
        let failed = database.get_upload(upload_id).await.unwrap().unwrap();
        assert_eq!(failed.state(), UploadState::Failed);
        assert_eq!(failed.failure_reason(), Some("checksum mismatch"));
        assert!(matches!(
            database.mark_failed(upload_id, "again").await,
            Err(DbError::InvalidTransition)
        ));
        assert!(matches!(
            database
                .mark_failed(id("0198f67e-9c0b-7000-8000-000000000099"), "missing")
                .await,
            Err(DbError::NotFound)
        ));

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn failure_reason_rejects_control_characters_and_more_than_500_characters() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000100");
        database
            .create_project(
                NewProject::with_created_at(
                    project_id,
                    "Reason validation",
                    "2026-08-04T10:00:00Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let control_id = id("0198f67e-9c0b-7000-8000-000000000101");
        let long_id = id("0198f67e-9c0b-7000-8000-000000000102");
        for upload_id in [control_id, long_id] {
            database
                .create_upload(
                    NewUpload::with_timestamps(
                        upload_id,
                        project_id,
                        "reason.bin",
                        0,
                        "2026-08-04T10:00:01Z",
                        "2026-08-04T10:00:01Z",
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
        }

        assert!(matches!(
            database.mark_failed(control_id, "line one\nline two").await,
            Err(DbError::InvalidFailureReason)
        ));
        let too_long = "x".repeat(MAX_FAILURE_REASON_CHARS + 1);
        assert!(matches!(
            database.mark_failed(long_id, &too_long).await,
            Err(DbError::InvalidFailureReason)
        ));
        assert_eq!(
            database
                .get_upload(control_id)
                .await
                .unwrap()
                .unwrap()
                .state(),
            UploadState::Active
        );
        assert_eq!(
            database.get_upload(long_id).await.unwrap().unwrap().state(),
            UploadState::Active
        );

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn finalizing_upload_can_transition_to_failed() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000110");
        database
            .create_project(
                NewProject::with_created_at(
                    project_id,
                    "Finalizing failure",
                    "2026-08-04T10:00:00Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let upload_id = id("0198f67e-9c0b-7000-8000-000000000111");
        database
            .create_upload(
                NewUpload::with_timestamps(
                    upload_id,
                    project_id,
                    "finalizing.bin",
                    0,
                    "2026-08-04T10:00:01Z",
                    "2026-08-04T10:00:01Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(database.mark_finalizing(upload_id, 0).await.unwrap());

        database
            .mark_failed(upload_id, "final move failed")
            .await
            .unwrap();
        assert_eq!(
            database
                .get_upload(upload_id)
                .await
                .unwrap()
                .unwrap()
                .state(),
            UploadState::Failed
        );

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn recoverable_uploads_include_only_active_and_finalizing_in_stable_order() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000120");
        database
            .create_project(
                NewProject::with_created_at(project_id, "Recovery", "2026-08-04T10:00:00Z")
                    .unwrap(),
            )
            .await
            .unwrap();
        let active_id = id("0198f67e-9c0b-7000-8000-000000000121");
        let finalizing_id = id("0198f67e-9c0b-7000-8000-000000000122");
        let complete_id = id("0198f67e-9c0b-7000-8000-000000000123");
        let failed_id = id("0198f67e-9c0b-7000-8000-000000000124");
        for (upload_id, created_at) in [
            (failed_id, "2026-08-04T10:00:00Z"),
            (complete_id, "2026-08-04T10:00:00Z"),
            (finalizing_id, "2026-08-04T10:00:01Z"),
            (active_id, "2026-08-04T10:00:01Z"),
        ] {
            database
                .create_upload(
                    NewUpload::with_timestamps(
                        upload_id,
                        project_id,
                        "recovery.bin",
                        0,
                        created_at,
                        created_at,
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
        }
        assert!(database.mark_finalizing(finalizing_id, 0).await.unwrap());
        assert!(database.mark_finalizing(complete_id, 0).await.unwrap());
        database.mark_complete(complete_id).await.unwrap();
        database.mark_failed(failed_id, "failed").await.unwrap();

        let recoverable = database.recoverable_uploads().await.unwrap();
        assert_eq!(
            recoverable.iter().map(|row| row.id()).collect::<Vec<_>>(),
            vec![active_id, finalizing_id]
        );
        assert_eq!(recoverable[0].state(), UploadState::Active);
        assert_eq!(recoverable[1].state(), UploadState::Finalizing);

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn recoverable_order_is_chronological_across_fractional_seconds() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000125");
        database
            .create_project(
                NewProject::with_created_at(
                    project_id,
                    "Fractional recovery",
                    "2026-08-04T10:00:00Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let whole_second = id("0198f67e-9c0b-7000-8000-000000000126");
        let fractional = id("0198f67e-9c0b-7000-8000-000000000127");
        for (upload_id, created_at) in [
            (fractional, "2026-08-04T10:00:00.1Z"),
            (whole_second, "2026-08-04T10:00:00Z"),
        ] {
            database
                .create_upload(
                    NewUpload::with_timestamps(
                        upload_id,
                        project_id,
                        "fractional.bin",
                        0,
                        created_at,
                        created_at,
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
        }

        assert_eq!(
            database
                .recoverable_uploads()
                .await
                .unwrap()
                .iter()
                .map(|row| row.id())
                .collect::<Vec<_>>(),
            vec![whole_second, fractional]
        );

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn corrupt_upload_state_returns_a_decoding_error() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000130");
        database
            .create_project(
                NewProject::with_created_at(project_id, "Corrupt state", "2026-08-04T10:00:00Z")
                    .unwrap(),
            )
            .await
            .unwrap();
        let upload_id = id("0198f67e-9c0b-7000-8000-000000000131");
        database
            .create_upload(
                NewUpload::with_timestamps(
                    upload_id,
                    project_id,
                    "corrupt.bin",
                    1,
                    "2026-08-04T10:00:01Z",
                    "2026-08-04T10:00:01Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let mut connection = database.pool.acquire().await.unwrap();
        sqlx::query("PRAGMA ignore_check_constraints = ON")
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query("UPDATE upload_session SET state = 'unknown' WHERE id = ?")
            .bind(upload_id.to_string())
            .execute(&mut *connection)
            .await
            .unwrap();
        drop(connection);

        assert!(matches!(
            database.get_upload(upload_id).await,
            Err(DbError::CorruptData)
        ));

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn corrupt_negative_upload_number_returns_a_decoding_error() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000140");
        database
            .create_project(
                NewProject::with_created_at(project_id, "Corrupt number", "2026-08-04T10:00:00Z")
                    .unwrap(),
            )
            .await
            .unwrap();
        let upload_id = id("0198f67e-9c0b-7000-8000-000000000141");
        database
            .create_upload(
                NewUpload::with_timestamps(
                    upload_id,
                    project_id,
                    "corrupt.bin",
                    1,
                    "2026-08-04T10:00:01Z",
                    "2026-08-04T10:00:01Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let mut connection = database.pool.acquire().await.unwrap();
        sqlx::query("PRAGMA ignore_check_constraints = ON")
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query("UPDATE upload_session SET committed_offset = -1 WHERE id = ?")
            .bind(upload_id.to_string())
            .execute(&mut *connection)
            .await
            .unwrap();
        drop(connection);

        assert!(matches!(
            database.get_upload(upload_id).await,
            Err(DbError::CorruptData)
        ));

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn corrupt_offset_above_total_size_returns_a_decoding_error() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000145");
        database
            .create_project(
                NewProject::with_created_at(project_id, "Corrupt offset", "2026-08-04T10:00:00Z")
                    .unwrap(),
            )
            .await
            .unwrap();
        let upload_id = id("0198f67e-9c0b-7000-8000-000000000146");
        database
            .create_upload(
                NewUpload::with_timestamps(
                    upload_id,
                    project_id,
                    "corrupt.bin",
                    1,
                    "2026-08-04T10:00:01Z",
                    "2026-08-04T10:00:01Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE upload_session SET committed_offset = 2 WHERE id = ?")
            .bind(upload_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();

        assert!(matches!(
            database.get_upload(upload_id).await,
            Err(DbError::CorruptData)
        ));

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn corrupt_upload_uuid_returns_a_decoding_error() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000150");
        database
            .create_project(
                NewProject::with_created_at(project_id, "Corrupt UUID", "2026-08-04T10:00:00Z")
                    .unwrap(),
            )
            .await
            .unwrap();
        let upload_id = id("0198f67e-9c0b-7000-8000-000000000151");
        database
            .create_upload(
                NewUpload::with_timestamps(
                    upload_id,
                    project_id,
                    "corrupt.bin",
                    1,
                    "2026-08-04T10:00:01Z",
                    "2026-08-04T10:00:01Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let mut connection = database.pool.acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query("UPDATE upload_session SET project_id = 'not-a-uuid' WHERE id = ?")
            .bind(upload_id.to_string())
            .execute(&mut *connection)
            .await
            .unwrap();
        drop(connection);

        assert!(matches!(
            database.get_upload(upload_id).await,
            Err(DbError::CorruptData)
        ));

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn corrupt_stored_timestamp_returns_a_decoding_error() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000160");
        database
            .create_project(
                NewProject::with_created_at(
                    project_id,
                    "Corrupt timestamp",
                    "2026-08-04T10:00:00Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE project SET created_at = 'not-a-timestamp' WHERE id = ?")
            .bind(project_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();

        assert!(matches!(
            database.get_project(project_id).await,
            Err(DbError::CorruptData)
        ));

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn get_project_maps_wrong_storage_class_to_corrupt_data() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000170");
        database
            .create_project(
                NewProject::with_created_at(project_id, "Wrong class", "2026-08-04T10:00:00Z")
                    .unwrap(),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE project SET name = X'80' WHERE id = ?")
            .bind(project_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();

        assert!(matches!(
            database.get_project(project_id).await,
            Err(DbError::CorruptData)
        ));

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn list_projects_maps_wrong_storage_class_to_corrupt_data() {
        let (directory, database) = test_database().await;
        sqlx::query(
            "INSERT INTO project (id, name, created_at) \
             VALUES (NULL, 'Null identifier', '2026-08-04T10:00:00Z')",
        )
        .execute(&database.pool)
        .await
        .unwrap();

        assert!(matches!(
            database.list_projects().await,
            Err(DbError::CorruptData)
        ));

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn get_upload_maps_wrong_storage_class_to_corrupt_data() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000172");
        database
            .create_project(
                NewProject::with_created_at(
                    project_id,
                    "Wrong upload class",
                    "2026-08-04T10:00:00Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let upload_id = id("0198f67e-9c0b-7000-8000-000000000173");
        database
            .create_upload(
                NewUpload::with_timestamps(
                    upload_id,
                    project_id,
                    "wrong-class.bin",
                    1,
                    "2026-08-04T10:00:01Z",
                    "2026-08-04T10:00:01Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE upload_session SET total_size = 1.5 WHERE id = ?")
            .bind(upload_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();

        assert!(matches!(
            database.get_upload(upload_id).await,
            Err(DbError::CorruptData)
        ));

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn recoverable_uploads_map_wrong_storage_class_to_corrupt_data() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000174");
        database
            .create_project(
                NewProject::with_created_at(
                    project_id,
                    "Wrong recovery class",
                    "2026-08-04T10:00:00Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let upload_id = id("0198f67e-9c0b-7000-8000-000000000175");
        database
            .create_upload(
                NewUpload::with_timestamps(
                    upload_id,
                    project_id,
                    "wrong-recovery-class.bin",
                    1,
                    "2026-08-04T10:00:01Z",
                    "2026-08-04T10:00:01Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE upload_session SET committed_offset = 'not-an-integer' WHERE id = ?")
            .bind(upload_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();

        assert!(matches!(
            database.recoverable_uploads().await,
            Err(DbError::CorruptData)
        ));

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn duplicate_upload_id_returns_a_safe_conflict_error() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000043");
        database
            .create_project(
                NewProject::with_created_at(project_id, "Duplicate upload", "2026-08-04T10:00:00Z")
                    .unwrap(),
            )
            .await
            .unwrap();
        let upload_id = id("0198f67e-9c0b-7000-8000-000000000044");
        for attempt in 0..2 {
            let result = database
                .create_upload(
                    NewUpload::with_timestamps(
                        upload_id,
                        project_id,
                        format!("attempt-{attempt}.bin"),
                        1,
                        "2026-08-04T10:00:01Z",
                        "2026-08-04T10:00:01Z",
                    )
                    .unwrap(),
                )
                .await;
            if attempt == 0 {
                result.unwrap();
            } else {
                assert!(matches!(result, Err(DbError::Conflict)));
            }
        }

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn upload_for_missing_project_returns_not_found() {
        let (directory, database) = test_database().await;
        let result = database
            .create_upload(
                NewUpload::with_timestamps(
                    id("0198f67e-9c0b-7000-8000-000000000045"),
                    id("0198f67e-9c0b-7000-8000-000000000046"),
                    "orphan.bin",
                    1,
                    "2026-08-04T10:00:01Z",
                    "2026-08-04T10:00:01Z",
                )
                .unwrap(),
            )
            .await;
        assert!(matches!(result, Err(DbError::NotFound)));

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn stored_invalid_failure_reasons_return_corrupt_data() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000112");
        database
            .create_project(
                NewProject::with_created_at(project_id, "Stored reasons", "2026-08-04T10:00:00Z")
                    .unwrap(),
            )
            .await
            .unwrap();
        let too_long_id = id("0198f67e-9c0b-7000-8000-000000000113");
        let control_id = id("0198f67e-9c0b-7000-8000-000000000114");
        for upload_id in [too_long_id, control_id] {
            database
                .create_upload(
                    NewUpload::with_timestamps(
                        upload_id,
                        project_id,
                        "stored-reason.bin",
                        0,
                        "2026-08-04T10:00:01Z",
                        "2026-08-04T10:00:01Z",
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
        }
        sqlx::query("UPDATE upload_session SET failure_reason = ? WHERE id = ?")
            .bind("x".repeat(MAX_FAILURE_REASON_CHARS + 1))
            .bind(too_long_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE upload_session SET failure_reason = ? WHERE id = ?")
            .bind("line one\nline two")
            .bind(control_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();

        for upload_id in [too_long_id, control_id] {
            assert!(matches!(
                database.get_upload(upload_id).await,
                Err(DbError::CorruptData)
            ));
        }

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn concurrent_offset_advances_have_exactly_one_winner() {
        let (directory, database) = test_database().await;
        let project_id = id("0198f67e-9c0b-7000-8000-000000000052");
        database
            .create_project(
                NewProject::with_created_at(
                    project_id,
                    "Concurrent offsets",
                    "2026-08-04T10:00:00Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let upload_id = id("0198f67e-9c0b-7000-8000-000000000053");
        database
            .create_upload(
                NewUpload::with_timestamps(
                    upload_id,
                    project_id,
                    "concurrent.bin",
                    64,
                    "2026-08-04T10:00:01Z",
                    "2026-08-04T10:00:01Z",
                )
                .unwrap(),
            )
            .await
            .unwrap();

        let first_database = database.clone();
        let second_database = database.clone();
        let (first, second) = tokio::join!(
            first_database.advance_offset(upload_id, 0, 32),
            second_database.advance_offset(upload_id, 0, 32),
        );
        let outcomes = [first.unwrap(), second.unwrap()];
        assert_eq!(outcomes.iter().filter(|outcome| **outcome).count(), 1);
        assert_eq!(
            database
                .get_upload(upload_id)
                .await
                .unwrap()
                .unwrap()
                .committed_offset(),
            32
        );

        database.close().await;
        drop(directory);
    }

    #[tokio::test]
    async fn query_execution_failure_remains_a_sql_error() {
        let (directory, database) = test_database().await;
        database.pool.close().await;

        assert!(matches!(
            database
                .get_project(id("0198f67e-9c0b-7000-8000-000000000176"))
                .await,
            Err(DbError::Sql(_))
        ));

        database.close().await;
        drop(directory);
    }
}

use async_trait::async_trait;
use cellar_core::{
    PendingChunk, ProjectId, UploadId, UploadRepository, UploadRepositoryError, UploadSession,
    UploadState,
};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};

const FIXED_UTC_TIMESTAMP_FORMAT: &str =
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:9]Z";

#[derive(Clone)]
pub struct SqliteUploadRepository {
    pool: SqlitePool,
}

impl SqliteUploadRepository {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl UploadRepository for SqliteUploadRepository {
    async fn create(
        &self,
        session: &UploadSession,
        max_active_sessions: u32,
        reservation_capacity: i64,
    ) -> Result<(), UploadRepositoryError> {
        if session.state != UploadState::Created
            || session.committed_offset != 0
            || session.pending.is_some()
            || session.expected_size < 0
            || reservation_capacity < 0
        {
            return Err(UploadRepositoryError::Unavailable);
        }
        let mut tx = self.pool.begin().await.map_err(map_sql)?;
        let project: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT status, deleted_at FROM project WHERE id = ?")
                .bind(session.project_id.to_string())
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sql)?;
        if !matches!(project, Some((ref status, None)) if status == "active") {
            return Err(UploadRepositoryError::NotFound);
        }
        if let Some(parent_id) = session.destination_parent_id {
            let valid_parent: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM file_entry
                 WHERE id = ? AND project_id = ? AND kind = 'directory' AND state = 'live'",
            )
            .bind(parent_id.to_string())
            .bind(session.project_id.to_string())
            .fetch_one(&mut *tx)
            .await
            .map_err(map_sql)?;
            if valid_parent != 1 {
                return Err(UploadRepositoryError::NotFound);
            }
        }
        let (active, reserved): (i64, i64) = sqlx::query_as(
            "SELECT count(*), COALESCE(sum(expected_size - committed_offset), 0)
             FROM upload_session
             WHERE state IN ('created', 'uploading', 'verifying', 'committing')",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sql)?;
        if active >= i64::from(max_active_sessions) {
            return Err(UploadRepositoryError::TooManySessions);
        }
        if reserved
            .checked_add(session.expected_size)
            .filter(|total| *total <= reservation_capacity)
            .is_none()
        {
            return Err(UploadRepositoryError::InsufficientStorage);
        }
        let result = sqlx::query(
            "INSERT INTO upload_session
             (id, project_id, destination_parent_id, destination_name, expected_size,
              committed_offset, expected_hash, state, expires_at)
             VALUES (?, ?, ?, ?, ?, 0, ?, 'created', ?)",
        )
        .bind(session.id.to_string())
        .bind(session.project_id.to_string())
        .bind(session.destination_parent_id.map(|id| id.to_string()))
        .bind(&session.destination_name)
        .bind(session.expected_size)
        .bind(session.expected_hash.map(Vec::from))
        .bind(timestamp(session.expires_at)?)
        .execute(&mut *tx)
        .await;
        match result {
            Ok(_) => tx.commit().await.map_err(map_sql),
            Err(error) if is_constraint(&error) => Err(UploadRepositoryError::Conflict),
            Err(error) => Err(map_sql(error)),
        }
    }

    async fn read(&self, id: UploadId) -> Result<UploadSession, UploadRepositoryError> {
        let row = sqlx::query("SELECT * FROM upload_session WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sql)?
            .ok_or(UploadRepositoryError::NotFound)?;
        row_to_session(&row)
    }

    async fn prepare_chunk(
        &self,
        id: UploadId,
        offset: i64,
        length: i64,
        digest: [u8; 32],
        max_concurrent_uploads: u32,
    ) -> Result<UploadSession, UploadRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(map_sql)?;
        let row = sqlx::query("SELECT * FROM upload_session WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sql)?
            .ok_or(UploadRepositoryError::NotFound)?;
        let session = row_to_session(&row)?;
        if !matches!(session.state, UploadState::Created | UploadState::Uploading)
            || session.pending.is_some()
            || session.committed_offset != offset
            || length <= 0
            || offset
                .checked_add(length)
                .is_none_or(|end| end > session.expected_size)
        {
            return Err(UploadRepositoryError::Conflict);
        }
        if session.state == UploadState::Created {
            let uploading: i64 =
                sqlx::query_scalar("SELECT count(*) FROM upload_session WHERE state = 'uploading'")
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(map_sql)?;
            if uploading >= i64::from(max_concurrent_uploads) {
                return Err(UploadRepositoryError::TooManyConcurrent);
            }
        }
        let updated = sqlx::query(
            "UPDATE upload_session
             SET pending_offset = ?, pending_length = ?, pending_digest = ?, state = 'uploading'
             WHERE id = ? AND committed_offset = ? AND pending_offset IS NULL
               AND state IN ('created', 'uploading')",
        )
        .bind(offset)
        .bind(length)
        .bind(Vec::from(digest))
        .bind(id.to_string())
        .bind(offset)
        .execute(&mut *tx)
        .await
        .map_err(map_sql)?
        .rows_affected();
        if updated != 1 {
            return Err(UploadRepositoryError::Conflict);
        }
        tx.commit().await.map_err(map_sql)?;
        self.read(id).await
    }

    async fn commit_chunk(
        &self,
        id: UploadId,
        offset: i64,
        length: i64,
        digest: [u8; 32],
    ) -> Result<UploadSession, UploadRepositoryError> {
        let end = offset
            .checked_add(length)
            .ok_or(UploadRepositoryError::Conflict)?;
        let updated = sqlx::query(
            "UPDATE upload_session
             SET committed_offset = ?, pending_offset = NULL, pending_length = NULL,
                 pending_digest = NULL
             WHERE id = ? AND committed_offset = ? AND pending_offset = ?
               AND pending_length = ? AND pending_digest = ? AND state = 'uploading'",
        )
        .bind(end)
        .bind(id.to_string())
        .bind(offset)
        .bind(offset)
        .bind(length)
        .bind(Vec::from(digest))
        .execute(&self.pool)
        .await
        .map_err(map_sql)?
        .rows_affected();
        if updated != 1 {
            return Err(UploadRepositoryError::Conflict);
        }
        self.read(id).await
    }

    async fn clear_pending(&self, id: UploadId) -> Result<UploadSession, UploadRepositoryError> {
        let updated = sqlx::query(
            "UPDATE upload_session
             SET pending_offset = NULL, pending_length = NULL, pending_digest = NULL
             WHERE id = ? AND pending_offset IS NOT NULL
               AND state IN ('created', 'uploading')",
        )
        .bind(id.to_string())
        .execute(&self.pool)
        .await
        .map_err(map_sql)?
        .rows_affected();
        if updated != 1 {
            return Err(UploadRepositoryError::Conflict);
        }
        self.read(id).await
    }

    async fn fail(&self, id: UploadId) -> Result<(), UploadRepositoryError> {
        transition_terminal(&self.pool, id, "failed").await
    }

    async fn cancel(&self, id: UploadId) -> Result<(), UploadRepositoryError> {
        transition_terminal(&self.pool, id, "cancelled").await
    }
}

async fn transition_terminal(
    pool: &SqlitePool,
    id: UploadId,
    state: &'static str,
) -> Result<(), UploadRepositoryError> {
    let updated = sqlx::query(
        "UPDATE upload_session SET state = ?, pending_offset = NULL,
         pending_length = NULL, pending_digest = NULL
         WHERE id = ? AND state IN ('created', 'uploading', 'verifying', 'committing')",
    )
    .bind(state)
    .bind(id.to_string())
    .execute(pool)
    .await
    .map_err(map_sql)?
    .rows_affected();
    if updated == 1 {
        return Ok(());
    }
    let existing: Option<String> =
        sqlx::query_scalar("SELECT state FROM upload_session WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(pool)
            .await
            .map_err(map_sql)?;
    match existing.as_deref() {
        None => Err(UploadRepositoryError::NotFound),
        Some(current) if current == state => Ok(()),
        Some(_) => Err(UploadRepositoryError::Conflict),
    }
}

fn row_to_session(row: &SqliteRow) -> Result<UploadSession, UploadRepositoryError> {
    let raw_id: String = row.try_get("id").map_err(map_sql)?;
    let id: UploadId = canonical_id(&raw_id)?;
    let raw_project_id: String = row.try_get("project_id").map_err(map_sql)?;
    let project_id: ProjectId = canonical_id(&raw_project_id)?;
    let raw_parent: Option<String> = row.try_get("destination_parent_id").map_err(map_sql)?;
    let destination_parent_id = raw_parent.as_deref().map(canonical_id).transpose()?;
    let expected_hash = digest(row.try_get("expected_hash").map_err(map_sql)?)?;
    let pending_offset: Option<i64> = row.try_get("pending_offset").map_err(map_sql)?;
    let pending_length: Option<i64> = row.try_get("pending_length").map_err(map_sql)?;
    let pending_digest = digest(row.try_get("pending_digest").map_err(map_sql)?)?;
    let pending = match (pending_offset, pending_length, pending_digest) {
        (None, None, None) => None,
        (Some(offset), Some(length), Some(digest)) => Some(PendingChunk {
            offset,
            length,
            digest,
        }),
        _ => return Err(UploadRepositoryError::Unavailable),
    };
    let state: String = row.try_get("state").map_err(map_sql)?;
    Ok(UploadSession {
        id,
        project_id,
        destination_parent_id,
        destination_name: row.try_get("destination_name").map_err(map_sql)?,
        expected_size: row.try_get("expected_size").map_err(map_sql)?,
        committed_offset: row.try_get("committed_offset").map_err(map_sql)?,
        expected_hash,
        pending,
        state: parse_state(&state)?,
        expires_at: parse_timestamp(&row.try_get::<String, _>("expires_at").map_err(map_sql)?)?,
    })
}

fn canonical_id<T>(raw: &str) -> Result<T, UploadRepositoryError>
where
    T: std::str::FromStr + ToString,
{
    let parsed: T = raw
        .parse()
        .map_err(|_| UploadRepositoryError::Unavailable)?;
    if parsed.to_string() != raw {
        return Err(UploadRepositoryError::Unavailable);
    }
    Ok(parsed)
}

fn digest(value: Option<Vec<u8>>) -> Result<Option<[u8; 32]>, UploadRepositoryError> {
    value
        .map(|value| {
            value
                .try_into()
                .map_err(|_| UploadRepositoryError::Unavailable)
        })
        .transpose()
}

fn parse_state(value: &str) -> Result<UploadState, UploadRepositoryError> {
    match value {
        "created" => Ok(UploadState::Created),
        "uploading" => Ok(UploadState::Uploading),
        "verifying" => Ok(UploadState::Verifying),
        "committing" => Ok(UploadState::Committing),
        "complete" => Ok(UploadState::Complete),
        "failed" => Ok(UploadState::Failed),
        "cancelled" => Ok(UploadState::Cancelled),
        _ => Err(UploadRepositoryError::Unavailable),
    }
}

fn timestamp(value: OffsetDateTime) -> Result<String, UploadRepositoryError> {
    let format = time::format_description::parse_borrowed::<2>(FIXED_UTC_TIMESTAMP_FORMAT)
        .map_err(|_| UploadRepositoryError::Unavailable)?;
    value
        .to_offset(UtcOffset::UTC)
        .format(&format)
        .map_err(|_| UploadRepositoryError::Unavailable)
}

fn parse_timestamp(value: &str) -> Result<OffsetDateTime, UploadRepositoryError> {
    let format = time::format_description::parse_borrowed::<2>(FIXED_UTC_TIMESTAMP_FORMAT)
        .map_err(|_| UploadRepositoryError::Unavailable)?;
    PrimitiveDateTime::parse(value, &format)
        .map(PrimitiveDateTime::assume_utc)
        .map_err(|_| UploadRepositoryError::Unavailable)
}

fn is_constraint(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(database) if database.is_unique_violation() || database.is_foreign_key_violation())
}

fn map_sql(_: sqlx::Error) -> UploadRepositoryError {
    UploadRepositoryError::Unavailable
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FilenameCollation, migrate, open_pool};
    use cellar_core::UploadRepository;
    use tempfile::TempDir;

    async fn repository() -> (TempDir, SqlitePool, SqliteUploadRepository, ProjectId) {
        let directory = TempDir::new().unwrap();
        let pool = open_pool(
            directory.path().join("cellar.db"),
            FilenameCollation::windows_ordinal_ci_v1(|left, right| left.cmp(right)),
        )
        .await
        .unwrap();
        migrate(&pool).await.unwrap();
        let project_id = ProjectId::new();
        sqlx::query(
            "INSERT INTO project
             (id, name, description, status, version, created_at, updated_at)
             VALUES (?, 'p', '', 'active', 1,
                     '1970-01-01T00:00:00.000000000Z',
                     '1970-01-01T00:00:00.000000000Z')",
        )
        .bind(project_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
        (
            directory,
            pool.clone(),
            SqliteUploadRepository::new(pool),
            project_id,
        )
    }

    fn session(project_id: ProjectId, name: &str, expected_size: i64) -> UploadSession {
        UploadSession {
            id: UploadId::new(),
            project_id,
            destination_parent_id: None,
            destination_name: name.to_owned(),
            expected_size,
            committed_offset: 0,
            expected_hash: None,
            pending: None,
            state: UploadState::Created,
            expires_at: OffsetDateTime::from_unix_timestamp(100).unwrap(),
        }
    }

    #[tokio::test]
    async fn chunk_commit_is_compare_and_swap_bound_to_offset_length_and_digest() {
        let (_directory, pool, repository, project_id) = repository().await;
        let session = session(project_id, "cas.bin", 8);
        repository.create(&session, 8, 100).await.unwrap();
        let digest = [7; 32];
        repository
            .prepare_chunk(session.id, 0, 4, digest, 3)
            .await
            .unwrap();
        assert_eq!(
            repository.commit_chunk(session.id, 0, 4, [8; 32]).await,
            Err(UploadRepositoryError::Conflict)
        );
        assert_eq!(
            repository.read(session.id).await.unwrap().committed_offset,
            0
        );
        let committed = repository
            .commit_chunk(session.id, 0, 4, digest)
            .await
            .unwrap();
        assert_eq!(committed.committed_offset, 4);
        assert!(committed.pending.is_none());
        assert_eq!(
            repository.commit_chunk(session.id, 0, 4, digest).await,
            Err(UploadRepositoryError::Conflict)
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn destination_session_and_concurrency_limits_are_transactional() {
        let (_directory, pool, first, project_id) = repository().await;
        let second = SqliteUploadRepository::new(pool.clone());
        let one = session(project_id, "one.bin", 10);
        let duplicate = session(project_id, "one.bin", 10);
        first.create(&one, 2, 20).await.unwrap();
        assert_eq!(
            second.create(&duplicate, 2, 20).await,
            Err(UploadRepositoryError::Conflict)
        );
        let two = session(project_id, "two.bin", 10);
        assert_eq!(
            second.create(&two, 2, 19).await,
            Err(UploadRepositoryError::InsufficientStorage)
        );
        second.create(&two, 2, 20).await.unwrap();
        assert_eq!(
            first
                .create(&session(project_id, "three.bin", 0), 2, 20)
                .await,
            Err(UploadRepositoryError::TooManySessions)
        );
        first.prepare_chunk(one.id, 0, 1, [1; 32], 1).await.unwrap();
        assert_eq!(
            second.prepare_chunk(two.id, 0, 1, [2; 32], 1).await,
            Err(UploadRepositoryError::TooManyConcurrent)
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn corrupt_durable_timestamps_fail_closed() {
        let (_directory, pool, repository, project_id) = repository().await;
        let upload = session(project_id, "corrupt.bin", 1);
        repository.create(&upload, 8, 100).await.unwrap();
        sqlx::query("UPDATE upload_session SET expires_at = ? WHERE id = ?")
            .bind("private-path-or-invalid-time")
            .bind(upload.id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            repository.read(upload.id).await,
            Err(UploadRepositoryError::Unavailable)
        );
        pool.close().await;
    }
}

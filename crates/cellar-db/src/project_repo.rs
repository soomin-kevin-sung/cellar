use async_trait::async_trait;
use cellar_core::{
    OperationId, OperationStart, Project, ProjectDescription, ProjectId, ProjectListFilter,
    ProjectName, ProjectPatch, ProjectRepository, ProjectRepositoryError, ProjectStatus,
};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};

#[derive(Clone)]
pub struct SqliteProjectRepository {
    pool: SqlitePool,
}

impl SqliteProjectRepository {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

const FIXED_UTC_TIMESTAMP_FORMAT: &str =
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:9]Z";
const MAX_REQUEST_DIGEST_BYTES: usize = 128;

#[derive(Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreatePayload<'a> {
    project_id: String,
    name: &'a str,
    description: &'a str,
    status: ProjectStatus,
    version: i64,
    created_at: String,
    updated_at: String,
    request_digest: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredCreatePayload {
    project_id: String,
    name: String,
    description: String,
    status: ProjectStatus,
    version: i64,
    created_at: String,
    updated_at: String,
    request_digest: String,
}

pub struct RecoveredProjectCreate {
    pub project: Project,
    request_digest: String,
}

impl RecoveredProjectCreate {
    #[must_use]
    pub fn request_digest(&self) -> &str {
        &self.request_digest
    }
}

impl std::fmt::Debug for RecoveredProjectCreate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecoveredProjectCreate")
            .field("project", &self.project)
            .field("request_digest", &"<redacted>")
            .finish()
    }
}

pub fn decode_project_create_payload(
    payload: &str,
) -> Result<RecoveredProjectCreate, ProjectRepositoryError> {
    let payload: StoredCreatePayload =
        serde_json::from_str(payload).map_err(|_| ProjectRepositoryError::Unavailable)?;
    if payload.request_digest.is_empty()
        || payload.request_digest.len() > MAX_REQUEST_DIGEST_BYTES
        || !payload
            .request_digest
            .bytes()
            .all(|byte| byte.is_ascii_graphic())
        || payload.status != ProjectStatus::Active
        || payload.version != 1
    {
        return Err(ProjectRepositoryError::Unavailable);
    }
    let created_at = parse_timestamp(&payload.created_at)?;
    let updated_at = parse_timestamp(&payload.updated_at)?;
    if created_at != updated_at {
        return Err(ProjectRepositoryError::Unavailable);
    }
    Ok(RecoveredProjectCreate {
        project: Project {
            id: payload
                .project_id
                .parse()
                .map_err(|_| ProjectRepositoryError::Unavailable)?,
            name: ProjectName::parse(payload.name)
                .map_err(|_| ProjectRepositoryError::Unavailable)?,
            description: ProjectDescription::parse(payload.description)
                .map_err(|_| ProjectRepositoryError::Unavailable)?,
            status: payload.status,
            version: payload.version,
            created_at,
            updated_at,
            deleted_at: None,
        },
        request_digest: payload.request_digest,
    })
}

#[async_trait]
impl ProjectRepository for SqliteProjectRepository {
    async fn begin_create(
        &self,
        operation_id: OperationId,
        project: &Project,
        request_digest: &str,
    ) -> Result<OperationStart, ProjectRepositoryError> {
        let payload = serde_json::to_string(&CreatePayload {
            project_id: project.id.to_string(),
            name: project.name.as_str(),
            description: project.description.as_str(),
            status: project.status,
            version: project.version,
            created_at: timestamp(project.created_at)?,
            updated_at: timestamp(project.updated_at)?,
            request_digest,
        })
        .map_err(|_| ProjectRepositoryError::Unavailable)?;
        let now = timestamp(project.created_at)?;
        let inserted = sqlx::query(
            "INSERT INTO operation
             (id, project_id, kind, state, payload_version, payload, created_at, updated_at)
             VALUES (?, NULL, 'project_create', 'pending', 1, ?, ?, ?)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(operation_id.to_string())
        .bind(payload)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(map_sql)?
        .rows_affected();
        if inserted == 1 {
            return Ok(OperationStart::New);
        }

        let row = sqlx::query(
            "SELECT kind, state, payload_version, payload, error
             FROM operation WHERE id = ?",
        )
        .bind(operation_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sql)?
        .ok_or(ProjectRepositoryError::Unavailable)?;
        let kind: String = row.try_get("kind").map_err(map_sql)?;
        let state: String = row.try_get("state").map_err(map_sql)?;
        let payload_version: i64 = row.try_get("payload_version").map_err(map_sql)?;
        let payload: String = row.try_get("payload").map_err(map_sql)?;
        let operation_error: Option<String> = row.try_get("error").map_err(map_sql)?;
        if kind != "project_create" || payload_version != 1 {
            return Err(ProjectRepositoryError::Conflict);
        }
        let payload = decode_project_create_payload(&payload)?;
        if payload.request_digest() != request_digest {
            return Err(ProjectRepositoryError::Conflict);
        }
        match state.as_str() {
            "complete" => self
                .read(payload.project.id)
                .await
                .map(OperationStart::Completed),
            "pending" | "fs_applied" => Ok(OperationStart::InProgress),
            "failed" => match operation_error.as_deref() {
                Some("project_destination_conflict") => Ok(OperationStart::Failed(
                    cellar_core::DirectoryStoreError::Conflict,
                )),
                Some("project_storage_unavailable") => Ok(OperationStart::Failed(
                    cellar_core::DirectoryStoreError::Unavailable,
                )),
                _ => Err(ProjectRepositoryError::Unavailable),
            },
            _ => Err(ProjectRepositoryError::Unavailable),
        }
    }

    async fn mark_create_fs_applied(
        &self,
        operation_id: OperationId,
        now: OffsetDateTime,
    ) -> Result<(), ProjectRepositoryError> {
        let affected = sqlx::query(
            "UPDATE operation SET state = 'fs_applied', updated_at = ?
             WHERE id = ? AND kind = 'project_create' AND state = 'pending'",
        )
        .bind(timestamp(now)?)
        .bind(operation_id.to_string())
        .execute(&self.pool)
        .await
        .map_err(map_sql)?
        .rows_affected();
        if affected == 1 {
            Ok(())
        } else {
            Err(ProjectRepositoryError::Unavailable)
        }
    }

    async fn mark_create_failed(
        &self,
        operation_id: OperationId,
        error_code: &'static str,
        now: OffsetDateTime,
    ) -> Result<(), ProjectRepositoryError> {
        let affected = sqlx::query(
            "UPDATE operation SET state = 'failed', error = ?, updated_at = ?
             WHERE id = ? AND kind = 'project_create' AND state = 'pending'",
        )
        .bind(error_code)
        .bind(timestamp(now)?)
        .bind(operation_id.to_string())
        .execute(&self.pool)
        .await
        .map_err(map_sql)?
        .rows_affected();
        if affected == 1 {
            Ok(())
        } else {
            Err(ProjectRepositoryError::Unavailable)
        }
    }

    async fn create(
        &self,
        operation_id: OperationId,
        project: &Project,
    ) -> Result<Project, ProjectRepositoryError> {
        let recovered = self.recover_create(operation_id).await?;
        if recovered == *project {
            Ok(recovered)
        } else {
            Err(ProjectRepositoryError::Conflict)
        }
    }

    async fn recover_create(
        &self,
        operation_id: OperationId,
    ) -> Result<Project, ProjectRepositoryError> {
        let mut transaction = self.pool.begin().await.map_err(map_sql)?;
        let row = sqlx::query(
            "SELECT state, payload_version, payload FROM operation
             WHERE id = ? AND kind = 'project_create'",
        )
        .bind(operation_id.to_string())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(map_sql)?
        .ok_or(ProjectRepositoryError::NotFound)?;
        let state: String = row.try_get("state").map_err(map_sql)?;
        let payload_version: i64 = row.try_get("payload_version").map_err(map_sql)?;
        let payload: String = row.try_get("payload").map_err(map_sql)?;
        if payload_version != 1 {
            return Err(ProjectRepositoryError::Unavailable);
        }
        let recovered = decode_project_create_payload(&payload)?;
        if state == "complete" {
            let row = sqlx::query(
                "SELECT id, name, description, status, version, created_at, updated_at,
                        deleted_at
                 FROM project WHERE id = ? AND deleted_at IS NULL",
            )
            .bind(recovered.project.id.to_string())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sql)?
            .ok_or(ProjectRepositoryError::Unavailable)?;
            let project = project_from_row(&row)?;
            transaction.commit().await.map_err(map_sql)?;
            return Ok(project);
        }
        if state != "fs_applied" {
            return Err(ProjectRepositoryError::Unavailable);
        }

        let project = &recovered.project;
        sqlx::query(
            "INSERT INTO project
             (id, name, description, status, version, created_at, updated_at, deleted_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, NULL)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(project.id.to_string())
        .bind(project.name.as_str())
        .bind(project.description.as_str())
        .bind(project.status.as_str())
        .bind(project.version)
        .bind(timestamp(project.created_at)?)
        .bind(timestamp(project.updated_at)?)
        .execute(&mut *transaction)
        .await
        .map_err(map_sql)?;
        let row = sqlx::query(
            "SELECT id, name, description, status, version, created_at, updated_at, deleted_at
             FROM project WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(project.id.to_string())
        .fetch_one(&mut *transaction)
        .await
        .map_err(map_sql)?;
        if project_from_row(&row)? != *project {
            return Err(ProjectRepositoryError::Conflict);
        }
        let completed = sqlx::query(
            "UPDATE operation
             SET state = 'complete', project_id = ?, error = NULL, updated_at = ?
             WHERE id = ? AND kind = 'project_create' AND state = 'fs_applied'",
        )
        .bind(project.id.to_string())
        .bind(timestamp(project.updated_at)?)
        .bind(operation_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(map_sql)?
        .rows_affected();
        if completed != 1 {
            return Err(ProjectRepositoryError::Unavailable);
        }
        transaction.commit().await.map_err(map_sql)?;
        Ok(project.clone())
    }

    async fn read(&self, id: ProjectId) -> Result<Project, ProjectRepositoryError> {
        let row = sqlx::query(
            "SELECT id, name, description, status, version, created_at, updated_at, deleted_at
             FROM project WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sql)?
        .ok_or(ProjectRepositoryError::NotFound)?;
        project_from_row(&row)
    }

    async fn list(
        &self,
        filter: ProjectListFilter,
        limit: u32,
    ) -> Result<Vec<Project>, ProjectRepositoryError> {
        let limit = i64::from(limit.min(cellar_core::MAX_PROJECT_LIST_LIMIT));
        let rows = match filter {
            ProjectListFilter::All => {
                sqlx::query(
                    "SELECT id, name, description, status, version, created_at, updated_at,
                            deleted_at
                     FROM project WHERE deleted_at IS NULL
                     ORDER BY created_at ASC, id ASC LIMIT ?",
                )
                .bind(limit)
                .fetch_all(&self.pool)
                .await
            }
            ProjectListFilter::Status(status) => {
                sqlx::query(
                    "SELECT id, name, description, status, version, created_at, updated_at,
                            deleted_at
                     FROM project WHERE deleted_at IS NULL AND status = ?
                     ORDER BY created_at ASC, id ASC LIMIT ?",
                )
                .bind(status.as_str())
                .bind(limit)
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(map_sql)?;
        rows.iter().map(project_from_row).collect()
    }

    async fn update(
        &self,
        id: ProjectId,
        expected_version: i64,
        patch: &ProjectPatch,
        now: OffsetDateTime,
    ) -> Result<Project, ProjectRepositoryError> {
        let mut transaction = self.pool.begin().await.map_err(map_sql)?;
        let row = sqlx::query(
            "UPDATE project
             SET name = COALESCE(?, name), description = COALESCE(?, description),
                 version = version + 1, updated_at = ?
             WHERE id = ? AND version = ? AND deleted_at IS NULL
             RETURNING id, name, description, status, version, created_at, updated_at, deleted_at",
        )
        .bind(patch.name.as_ref().map(ProjectName::as_str))
        .bind(patch.description.as_ref().map(ProjectDescription::as_str))
        .bind(timestamp(now)?)
        .bind(id.to_string())
        .bind(expected_version)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(map_sql)?;
        let project = match row {
            Some(row) => project_from_row(&row)?,
            None => return Err(classify_cas_miss(&mut transaction, id).await?),
        };
        transaction.commit().await.map_err(map_sql)?;
        Ok(project)
    }

    async fn archive(
        &self,
        id: ProjectId,
        expected_version: i64,
        now: OffsetDateTime,
    ) -> Result<Project, ProjectRepositoryError> {
        let mut transaction = self.pool.begin().await.map_err(map_sql)?;
        let row = sqlx::query(
            "UPDATE project
             SET status = 'archived', version = version + 1, updated_at = ?
             WHERE id = ? AND version = ? AND deleted_at IS NULL
             RETURNING id, name, description, status, version, created_at, updated_at, deleted_at",
        )
        .bind(timestamp(now)?)
        .bind(id.to_string())
        .bind(expected_version)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(map_sql)?;
        let project = match row {
            Some(row) => project_from_row(&row)?,
            None => return Err(classify_cas_miss(&mut transaction, id).await?),
        };
        transaction.commit().await.map_err(map_sql)?;
        Ok(project)
    }
}

async fn classify_cas_miss(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: ProjectId,
) -> Result<ProjectRepositoryError, ProjectRepositoryError> {
    let exists: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM project WHERE id = ? AND deleted_at IS NULL)",
    )
    .bind(id.to_string())
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sql)?;
    Ok(if exists == 0 {
        ProjectRepositoryError::NotFound
    } else {
        ProjectRepositoryError::Stale
    })
}

fn project_from_row(row: &SqliteRow) -> Result<Project, ProjectRepositoryError> {
    let id: String = row.try_get("id").map_err(map_sql)?;
    let name: String = row.try_get("name").map_err(map_sql)?;
    let description: String = row.try_get("description").map_err(map_sql)?;
    let status: String = row.try_get("status").map_err(map_sql)?;
    let version: i64 = row.try_get("version").map_err(map_sql)?;
    let created_at: String = row.try_get("created_at").map_err(map_sql)?;
    let updated_at: String = row.try_get("updated_at").map_err(map_sql)?;
    let deleted_at: Option<String> = row.try_get("deleted_at").map_err(map_sql)?;
    Ok(Project {
        id: id
            .parse()
            .map_err(|_| ProjectRepositoryError::Unavailable)?,
        name: ProjectName::parse(name).map_err(|_| ProjectRepositoryError::Unavailable)?,
        description: ProjectDescription::parse(description)
            .map_err(|_| ProjectRepositoryError::Unavailable)?,
        status: match status.as_str() {
            "active" => ProjectStatus::Active,
            "archived" => ProjectStatus::Archived,
            _ => return Err(ProjectRepositoryError::Unavailable),
        },
        version,
        created_at: parse_timestamp(&created_at)?,
        updated_at: parse_timestamp(&updated_at)?,
        deleted_at: deleted_at.as_deref().map(parse_timestamp).transpose()?,
    })
}

fn timestamp(value: OffsetDateTime) -> Result<String, ProjectRepositoryError> {
    let format = time::format_description::parse_borrowed::<2>(FIXED_UTC_TIMESTAMP_FORMAT)
        .map_err(|_| ProjectRepositoryError::Unavailable)?;
    value
        .to_offset(UtcOffset::UTC)
        .format(&format)
        .map_err(|_| ProjectRepositoryError::Unavailable)
}

fn parse_timestamp(value: &str) -> Result<OffsetDateTime, ProjectRepositoryError> {
    let format = time::format_description::parse_borrowed::<2>(FIXED_UTC_TIMESTAMP_FORMAT)
        .map_err(|_| ProjectRepositoryError::Unavailable)?;
    PrimitiveDateTime::parse(value, &format)
        .map(PrimitiveDateTime::assume_utc)
        .map_err(|_| ProjectRepositoryError::Unavailable)
}

fn map_sql(_error: sqlx::Error) -> ProjectRepositoryError {
    ProjectRepositoryError::Unavailable
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FilenameCollation, migrate, open_pool};
    use tempfile::TempDir;

    async fn repository() -> (TempDir, SqlitePool, SqliteProjectRepository) {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("cellar.db");
        let collation = FilenameCollation::windows_ordinal_ci_v1(|left, right| left.cmp(right));
        let pool = open_pool(path, collation).await.unwrap();
        migrate(&pool).await.unwrap();
        let repository = SqliteProjectRepository::new(pool.clone());
        (directory, pool, repository)
    }

    async fn created(repository: &SqliteProjectRepository) -> Project {
        let now = OffsetDateTime::from_unix_timestamp(10).unwrap();
        let operation = OperationId::new();
        let id = ProjectId::new();
        let project = Project::from_new(
            id,
            cellar_core::NewProject::try_new("name", "description").unwrap(),
            now,
        );
        assert_eq!(
            repository
                .begin_create(operation, &project, "digest")
                .await
                .unwrap(),
            OperationStart::New
        );
        repository
            .mark_create_fs_applied(operation, now)
            .await
            .unwrap();
        repository.create(operation, &project).await.unwrap()
    }

    #[tokio::test]
    async fn lifecycle_uses_exact_versions_and_deterministic_lists() {
        let (_directory, pool, repository) = repository().await;
        let project = created(&repository).await;
        let now = OffsetDateTime::from_unix_timestamp(20).unwrap();
        let updated = repository
            .update(
                project.id,
                1,
                &ProjectPatch::try_new(Some("renamed".into()), None).unwrap(),
                now,
            )
            .await
            .unwrap();
        assert_eq!(updated.version, 2);
        assert_eq!(
            repository
                .update(
                    project.id,
                    1,
                    &ProjectPatch::try_new(None, Some("stale".into())).unwrap(),
                    now,
                )
                .await,
            Err(ProjectRepositoryError::Stale)
        );
        let archived = repository.archive(project.id, 2, now).await.unwrap();
        assert_eq!(archived.version, 3);
        assert_eq!(archived.status, ProjectStatus::Archived);
        assert!(
            repository
                .list(ProjectListFilter::Status(ProjectStatus::Active), 100)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            repository
                .list(ProjectListFilter::Status(ProjectStatus::Archived), 100)
                .await
                .unwrap(),
            vec![archived]
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn operation_replay_checks_digest_and_retains_fs_applied_evidence() {
        let (_directory, pool, repository) = repository().await;
        let project = created(&repository).await;
        let operation: String = sqlx::query_scalar(
            "SELECT id FROM operation WHERE project_id = ? AND state = 'complete'",
        )
        .bind(project.id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        let operation: OperationId = operation.parse().unwrap();
        assert_eq!(
            repository
                .begin_create(operation, &project, "digest")
                .await
                .unwrap(),
            OperationStart::Completed(project.clone())
        );
        assert_eq!(
            repository
                .begin_create(operation, &project, "different")
                .await,
            Err(ProjectRepositoryError::Conflict)
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn list_order_filters_and_soft_delete_exclusion_are_exact() {
        let (_directory, pool, repository) = repository().await;
        let early = ProjectId::new();
        let same_time_first = ProjectId::new();
        let same_time_second = ProjectId::new();
        let (lower_id, higher_id) = if same_time_first.to_string() < same_time_second.to_string() {
            (same_time_first, same_time_second)
        } else {
            (same_time_second, same_time_first)
        };
        let archived = ProjectId::new();
        let deleted = ProjectId::new();
        for (id, name, status, created_at, deleted_at) in [
            (
                early,
                "early",
                "active",
                "1970-01-01T00:00:01.000000000Z",
                None,
            ),
            (
                higher_id,
                "same-high",
                "active",
                "1970-01-01T00:00:02.000000000Z",
                None,
            ),
            (
                lower_id,
                "same-low",
                "active",
                "1970-01-01T00:00:02.000000000Z",
                None,
            ),
            (
                archived,
                "archived",
                "archived",
                "1970-01-01T00:00:03.000000000Z",
                None,
            ),
            (
                deleted,
                "deleted",
                "active",
                "1970-01-01T00:00:00.000000000Z",
                Some("1970-01-01T00:00:04.000000000Z"),
            ),
        ] {
            sqlx::query(
                "INSERT INTO project
                 (id, name, description, status, version, created_at, updated_at, deleted_at)
                 VALUES (?, ?, '', ?, 1, ?, ?, ?)",
            )
            .bind(id.to_string())
            .bind(name)
            .bind(status)
            .bind(created_at)
            .bind(created_at)
            .bind(deleted_at)
            .execute(&pool)
            .await
            .unwrap();
        }

        let active = repository
            .list(ProjectListFilter::Status(ProjectStatus::Active), 100)
            .await
            .unwrap();
        assert_eq!(
            active.iter().map(|project| project.id).collect::<Vec<_>>(),
            vec![early, lower_id, higher_id]
        );
        let all = repository.list(ProjectListFilter::All, 100).await.unwrap();
        assert_eq!(
            all.iter().map(|project| project.id).collect::<Vec<_>>(),
            vec![early, lower_id, higher_id, archived]
        );
        assert_eq!(
            repository
                .list(ProjectListFilter::Status(ProjectStatus::Archived), 100)
                .await
                .unwrap()
                .into_iter()
                .map(|project| project.id)
                .collect::<Vec<_>>(),
            vec![archived]
        );

        let now = OffsetDateTime::from_unix_timestamp(100).unwrap();
        assert_eq!(
            repository.read(deleted).await,
            Err(ProjectRepositoryError::NotFound)
        );
        assert_eq!(
            repository
                .update(
                    deleted,
                    1,
                    &ProjectPatch::try_new(Some("still-deleted".into()), None).unwrap(),
                    now,
                )
                .await,
            Err(ProjectRepositoryError::NotFound)
        );
        assert_eq!(
            repository.archive(deleted, 1, now).await,
            Err(ProjectRepositoryError::NotFound)
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn fs_applied_payload_reconstructs_and_recovers_the_exact_project() {
        let (_directory, pool, repository) = repository().await;
        let operation = OperationId::new();
        let project = Project::from_new(
            ProjectId::new(),
            cellar_core::NewProject::try_new("recover me", "line one\nline two").unwrap(),
            OffsetDateTime::from_unix_timestamp(5)
                .unwrap()
                .replace_nanosecond(123_456_789)
                .unwrap(),
        );
        assert_eq!(
            repository
                .begin_create(operation, &project, "body-digest")
                .await
                .unwrap(),
            OperationStart::New
        );
        repository
            .mark_create_fs_applied(operation, project.created_at)
            .await
            .unwrap();
        let payload: String = sqlx::query_scalar("SELECT payload FROM operation WHERE id = ?")
            .bind(operation.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        let recovered = decode_project_create_payload(&payload).unwrap();
        assert_eq!(recovered.project, project);
        assert_eq!(recovered.request_digest(), "body-digest");
        assert_eq!(
            payload,
            format!(
                concat!(
                    "{{\"projectId\":\"{}\",\"name\":\"recover me\",",
                    "\"description\":\"line one\\nline two\",\"status\":\"active\",",
                    "\"version\":1,\"createdAt\":\"1970-01-01T00:00:05.123456789Z\",",
                    "\"updatedAt\":\"1970-01-01T00:00:05.123456789Z\",",
                    "\"requestDigest\":\"body-digest\"}}"
                ),
                project.id
            )
        );
        let with_unknown = payload.replacen("{", "{\"absolutePath\":\"C:\\\\private\",", 1);
        assert_eq!(
            decode_project_create_payload(&with_unknown).unwrap_err(),
            ProjectRepositoryError::Unavailable
        );
        let variable_width = payload.replace("05.123456789Z", "05.123Z");
        assert_eq!(
            decode_project_create_payload(&variable_width).unwrap_err(),
            ProjectRepositoryError::Unavailable
        );

        assert_eq!(repository.recover_create(operation).await.unwrap(), project);
        assert_eq!(repository.recover_create(operation).await.unwrap(), project);
        pool.close().await;
    }

    #[tokio::test]
    async fn timestamps_are_fixed_width_utc_and_sort_chronologically() {
        let (_directory, pool, repository) = repository().await;
        let zero = OffsetDateTime::from_unix_timestamp(5).unwrap();
        let fraction = zero.replace_nanosecond(100_000_000).unwrap();
        assert_eq!(timestamp(zero).unwrap(), "1970-01-01T00:00:05.000000000Z");
        assert_eq!(
            timestamp(fraction).unwrap(),
            "1970-01-01T00:00:05.100000000Z"
        );

        let later = Project::from_new(
            ProjectId::new(),
            cellar_core::NewProject::try_new("later", "").unwrap(),
            fraction,
        );
        let earlier = Project::from_new(
            ProjectId::new(),
            cellar_core::NewProject::try_new("earlier", "").unwrap(),
            zero,
        );
        for project in [&later, &earlier] {
            let operation = OperationId::new();
            repository
                .begin_create(operation, project, project.name.as_str())
                .await
                .unwrap();
            repository
                .mark_create_fs_applied(operation, project.created_at)
                .await
                .unwrap();
            repository.create(operation, project).await.unwrap();
        }
        let listed = repository
            .list(ProjectListFilter::Status(ProjectStatus::Active), 100)
            .await
            .unwrap();
        assert_eq!(
            listed.iter().map(|project| project.id).collect::<Vec<_>>(),
            vec![earlier.id, later.id]
        );
        assert_eq!(listed, vec![earlier.clone(), later.clone()]);
        let stored: Vec<String> =
            sqlx::query_scalar("SELECT created_at FROM project ORDER BY created_at")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            stored,
            vec![
                "1970-01-01T00:00:05.000000000Z",
                "1970-01-01T00:00:05.100000000Z"
            ]
        );

        let update_time = OffsetDateTime::from_unix_timestamp(6)
            .unwrap()
            .replace_nanosecond(7)
            .unwrap();
        let updated = repository
            .update(
                earlier.id,
                1,
                &ProjectPatch::try_new(None, Some("updated".into())).unwrap(),
                update_time,
            )
            .await
            .unwrap();
        assert_eq!(updated.updated_at, update_time);
        assert_eq!(repository.read(earlier.id).await.unwrap(), updated);
        let stored_updated_at: String =
            sqlx::query_scalar("SELECT updated_at FROM project WHERE id = ?")
                .bind(earlier.id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored_updated_at, "1970-01-01T00:00:06.000000007Z");
        pool.close().await;
    }
}

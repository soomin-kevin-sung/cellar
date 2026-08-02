use async_trait::async_trait;
use cellar_core::{
    OperationId, OperationStart, Project, ProjectDescription, ProjectId, ProjectListFilter,
    ProjectName, ProjectPatch, ProjectRepository, ProjectRepositoryError, ProjectStatus,
};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

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

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreatePayload<'a> {
    project_id: String,
    request_digest: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredCreatePayload {
    project_id: String,
    request_digest: String,
}

#[async_trait]
impl ProjectRepository for SqliteProjectRepository {
    async fn begin_create(
        &self,
        operation_id: OperationId,
        project_id: ProjectId,
        request_digest: &str,
        now: OffsetDateTime,
    ) -> Result<OperationStart, ProjectRepositoryError> {
        let payload = serde_json::to_string(&CreatePayload {
            project_id: project_id.to_string(),
            request_digest,
        })
        .map_err(|_| ProjectRepositoryError::Unavailable)?;
        let now = timestamp(now)?;
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

        let row =
            sqlx::query("SELECT kind, state, payload_version, payload FROM operation WHERE id = ?")
                .bind(operation_id.to_string())
                .fetch_optional(&self.pool)
                .await
                .map_err(map_sql)?
                .ok_or(ProjectRepositoryError::Unavailable)?;
        let kind: String = row.try_get("kind").map_err(map_sql)?;
        let state: String = row.try_get("state").map_err(map_sql)?;
        let payload_version: i64 = row.try_get("payload_version").map_err(map_sql)?;
        let payload: String = row.try_get("payload").map_err(map_sql)?;
        if kind != "project_create" || payload_version != 1 {
            return Err(ProjectRepositoryError::Conflict);
        }
        let payload: StoredCreatePayload =
            serde_json::from_str(&payload).map_err(|_| ProjectRepositoryError::Unavailable)?;
        if payload.request_digest != request_digest {
            return Err(ProjectRepositoryError::Conflict);
        }
        match state.as_str() {
            "complete" => {
                let id = payload
                    .project_id
                    .parse()
                    .map_err(|_| ProjectRepositoryError::Unavailable)?;
                self.read(id).await.map(OperationStart::Completed)
            }
            "pending" | "fs_applied" | "failed" => Ok(OperationStart::InProgress),
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
        let mut transaction = self.pool.begin().await.map_err(map_sql)?;
        let inserted = sqlx::query(
            "INSERT INTO project
             (id, name, description, status, version, created_at, updated_at, deleted_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, NULL)",
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
        .map_err(map_insert_sql)?
        .rows_affected();
        if inserted != 1 {
            return Err(ProjectRepositoryError::Unavailable);
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
    value
        .to_offset(UtcOffset::UTC)
        .format(&Rfc3339)
        .map_err(|_| ProjectRepositoryError::Unavailable)
}

fn parse_timestamp(value: &str) -> Result<OffsetDateTime, ProjectRepositoryError> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| ProjectRepositoryError::Unavailable)
}

fn map_sql(_error: sqlx::Error) -> ProjectRepositoryError {
    ProjectRepositoryError::Unavailable
}

fn map_insert_sql(error: sqlx::Error) -> ProjectRepositoryError {
    if matches!(&error, sqlx::Error::Database(database) if database.is_unique_violation()) {
        ProjectRepositoryError::Conflict
    } else {
        ProjectRepositoryError::Unavailable
    }
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
        assert_eq!(
            repository
                .begin_create(operation, id, "digest", now)
                .await
                .unwrap(),
            OperationStart::New
        );
        repository
            .mark_create_fs_applied(operation, now)
            .await
            .unwrap();
        let project = Project::from_new(
            id,
            cellar_core::NewProject::try_new("name", "description").unwrap(),
            now,
        );
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
                .begin_create(operation, ProjectId::new(), "digest", project.created_at)
                .await
                .unwrap(),
            OperationStart::Completed(project.clone())
        );
        assert_eq!(
            repository
                .begin_create(operation, ProjectId::new(), "different", project.created_at)
                .await,
            Err(ProjectRepositoryError::Conflict)
        );
        pool.close().await;
    }
}

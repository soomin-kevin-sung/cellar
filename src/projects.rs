//! Project creation and listing.

use std::{collections::HashSet, future::Future, pin::Pin, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
    routing::get,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    app::RequestId,
    auth::OwnerIdentity,
    db::{Database, DbError, NewProject, ProjectRow},
    error::AppError,
    storage::{Storage, StorageError},
};

type RepositoryFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, DbError>> + Send + 'a>>;
type StorageFuture<'a> = Pin<Box<dyn Future<Output = Result<(), StorageError>> + Send + 'a>>;

/// The database operations required by the project API.
pub trait ProjectRepository: Send + Sync {
    fn create_project<'a>(&'a self, project: NewProject) -> RepositoryFuture<'a, ProjectRow>;
    fn list_projects<'a>(&'a self) -> RepositoryFuture<'a, Vec<ProjectRow>>;
}

impl ProjectRepository for Database {
    fn create_project<'a>(&'a self, project: NewProject) -> RepositoryFuture<'a, ProjectRow> {
        Box::pin(async move { self.create_project(project).await })
    }

    fn list_projects<'a>(&'a self) -> RepositoryFuture<'a, Vec<ProjectRow>> {
        Box::pin(async move { self.list_projects().await })
    }
}

/// The filesystem operations required by project creation.
pub trait ProjectStorage: Send + Sync {
    fn create_project_dir<'a>(&'a self, project_id: Uuid) -> StorageFuture<'a>;
    fn remove_empty_project_dir<'a>(&'a self, project_id: Uuid) -> StorageFuture<'a>;
}

impl ProjectStorage for Storage {
    fn create_project_dir<'a>(&'a self, project_id: Uuid) -> StorageFuture<'a> {
        Box::pin(async move { self.create_project_dir(project_id).await })
    }

    fn remove_empty_project_dir<'a>(&'a self, project_id: Uuid) -> StorageFuture<'a> {
        Box::pin(async move { self.remove_empty_project_dir(project_id).await })
    }
}

#[derive(Clone)]
pub struct ProjectService {
    repository: Arc<dyn ProjectRepository>,
    storage: Arc<dyn ProjectStorage>,
}

impl ProjectService {
    pub fn new(repository: Arc<dyn ProjectRepository>, storage: Arc<dyn ProjectStorage>) -> Self {
        Self {
            repository,
            storage,
        }
    }

    async fn create(
        &self,
        name: String,
        request_id: &RequestId,
    ) -> Result<ProjectRow, ProjectServiceError> {
        let name = validate_name(&name).ok_or(ProjectServiceError::InvalidRequest)?;
        let project_id = Uuid::now_v7();
        let project = NewProject::new(project_id, name).map_err(|_| {
            record_failure(
                ProjectFailureReason::TimestampUnavailable,
                request_id,
                project_id,
            );
            ProjectServiceError::CreateFailed
        })?;

        let repository = self.repository.clone();
        let storage = self.storage.clone();
        let operation_request_id = request_id.clone();
        tokio::spawn(async move {
            create_owned(
                repository,
                storage,
                project,
                project_id,
                operation_request_id,
            )
            .await
        })
        .await
        .unwrap_or_else(|_| {
            record_failure(
                ProjectFailureReason::CreateTaskFailed,
                request_id,
                project_id,
            );
            Err(ProjectServiceError::CreateFailed)
        })
    }

    async fn list(&self, request_id: &RequestId) -> Result<Vec<ProjectRow>, ProjectServiceError> {
        self.repository.list_projects().await.map_err(|_| {
            tracing::warn!(
                reason = "database_list_failed",
                request_id = %request_id,
                "project listing failed"
            );
            ProjectServiceError::ListFailed
        })
    }
}

/// A closed startup failure from comparing committed projects with managed storage.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum ProjectAuditError {
    #[error("project audit database query failed")]
    Database,
    #[error("project audit found unsafe managed storage")]
    Storage,
}

/// Reports safe, well-formed project directories that have no committed database row.
///
/// This audit is intentionally read-only: it never imports, removes, or repairs entries.
pub async fn audit_orphan_project_directories(
    database: &Database,
    storage: &Storage,
) -> Result<(), ProjectAuditError> {
    let committed = database
        .list_projects()
        .await
        .map_err(|_| ProjectAuditError::Database)?
        .into_iter()
        .map(|project| project.id())
        .collect::<HashSet<_>>();
    let project_directories = storage
        .scan_project_directories()
        .await
        .map_err(|_| ProjectAuditError::Storage)?;

    for project_id in project_directories {
        if !committed.contains(&project_id) {
            tracing::warn!(
                reason = "orphan_project_directory",
                project_id = %project_id,
                "unreferenced project directory requires manual cleanup"
            );
        }
    }
    Ok(())
}

async fn create_owned(
    repository: Arc<dyn ProjectRepository>,
    storage: Arc<dyn ProjectStorage>,
    project: NewProject,
    project_id: Uuid,
    request_id: RequestId,
) -> Result<ProjectRow, ProjectServiceError> {
    if let Err(error) = storage.create_project_dir(project_id).await {
        let result = match &error {
            StorageError::InsufficientSpace => ProjectServiceError::InsufficientStorage,
            StorageError::ProjectCleanupFailed { .. } => ProjectServiceError::CleanupFailed,
            _ => ProjectServiceError::CreateFailed,
        };
        record_failure(storage_failure_reason(&error), &request_id, project_id);
        return Err(result);
    }

    match repository.create_project(project).await {
        Ok(project) => Ok(project),
        Err(_) => match storage.remove_empty_project_dir(project_id).await {
            Ok(()) => {
                record_failure(
                    ProjectFailureReason::DatabaseInsertFailed,
                    &request_id,
                    project_id,
                );
                Err(ProjectServiceError::CreateFailed)
            }
            Err(_) => {
                record_failure(
                    ProjectFailureReason::DirectoryCleanupFailed,
                    &request_id,
                    project_id,
                );
                Err(ProjectServiceError::CleanupFailed)
            }
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectServiceError {
    InvalidRequest,
    CreateFailed,
    CleanupFailed,
    InsufficientStorage,
    ListFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectFailureReason {
    TimestampUnavailable,
    InsufficientSpace,
    StorageAlreadyExists,
    UnsafeStorage,
    StorageUnavailable,
    DatabaseInsertFailed,
    DirectoryCleanupFailed,
    CreateTaskFailed,
}

impl ProjectFailureReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::TimestampUnavailable => "timestamp_unavailable",
            Self::InsufficientSpace => "insufficient_space",
            Self::StorageAlreadyExists => "storage_already_exists",
            Self::UnsafeStorage => "unsafe_storage",
            Self::StorageUnavailable => "storage_unavailable",
            Self::DatabaseInsertFailed => "database_insert_failed",
            Self::DirectoryCleanupFailed => "directory_cleanup_failed",
            Self::CreateTaskFailed => "create_task_failed",
        }
    }
}

fn storage_failure_reason(error: &StorageError) -> ProjectFailureReason {
    match error {
        StorageError::InsufficientSpace => ProjectFailureReason::InsufficientSpace,
        StorageError::AlreadyExists => ProjectFailureReason::StorageAlreadyExists,
        StorageError::InvalidRoot(_)
        | StorageError::UnsafeManagedEntry
        | StorageError::UnsafeEntry => ProjectFailureReason::UnsafeStorage,
        StorageError::ProjectCleanupFailed { .. } => ProjectFailureReason::DirectoryCleanupFailed,
        StorageError::NotFound
        | StorageError::NonEmptyStaging
        | StorageError::InvalidBody
        | StorageError::OffsetMismatch { .. }
        | StorageError::AmbiguousCleanup { .. }
        | StorageError::Io { .. } => ProjectFailureReason::StorageUnavailable,
    }
}

fn record_failure(reason: ProjectFailureReason, request_id: &RequestId, project_id: Uuid) {
    tracing::warn!(
        reason = reason.as_str(),
        request_id = %request_id,
        project_id = %project_id,
        "project creation failed"
    );
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateProjectRequest {
    name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectResponse {
    id: Uuid,
    name: String,
    created_at: String,
}

impl From<ProjectRow> for ProjectResponse {
    fn from(project: ProjectRow) -> Self {
        Self {
            id: project.id(),
            name: project.name().to_owned(),
            created_at: project.created_at().to_owned(),
        }
    }
}

pub fn project_router(service: ProjectService) -> Router {
    Router::new()
        .route("/api/v1/projects", get(list_projects).post(create_project))
        .with_state(service)
}

async fn list_projects(
    State(service): State<ProjectService>,
    Extension(request_id): Extension<RequestId>,
    Extension(_owner): Extension<OwnerIdentity>,
) -> Result<Json<Vec<ProjectResponse>>, AppError> {
    service
        .list(&request_id)
        .await
        .map(|projects| Json(projects.into_iter().map(Into::into).collect()))
        .map_err(|_| {
            AppError::service_unavailable(
                request_id,
                "project_list_failed",
                "Projects are temporarily unavailable.",
            )
        })
}

async fn create_project(
    State(service): State<ProjectService>,
    Extension(request_id): Extension<RequestId>,
    Extension(_owner): Extension<OwnerIdentity>,
    payload: Result<Json<CreateProjectRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ProjectResponse>), AppError> {
    let Json(payload) = payload.map_err(|_| invalid_project(request_id.clone()))?;
    let project = service
        .create(payload.name, &request_id)
        .await
        .map_err(|error| match error {
            ProjectServiceError::InvalidRequest => invalid_project(request_id.clone()),
            ProjectServiceError::CreateFailed => AppError::service_unavailable(
                request_id.clone(),
                "project_create_failed",
                "Project creation is temporarily unavailable.",
            ),
            ProjectServiceError::CleanupFailed => AppError::service_unavailable(
                request_id.clone(),
                "project_cleanup_failed",
                "Project creation could not be completed safely.",
            ),
            ProjectServiceError::InsufficientStorage => {
                AppError::insufficient_storage(request_id.clone())
            }
            ProjectServiceError::ListFailed => unreachable!("create cannot return a list error"),
        })?;
    Ok((StatusCode::CREATED, Json(project.into())))
}

fn invalid_project(request_id: RequestId) -> AppError {
    AppError::bad_request(request_id, "invalid_request", "The request is invalid.")
}

fn validate_name(name: &str) -> Option<String> {
    let name = name.trim();
    let scalar_count = name.chars().count();
    if !(1..=100).contains(&scalar_count) || name.chars().any(char::is_control) {
        return None;
    }
    Some(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{ProjectFailureReason, validate_name};

    #[test]
    fn name_validation_counts_unicode_scalars_and_rejects_controls() {
        assert_eq!(
            validate_name(" \u{2003}Name\u{2003} ").as_deref(),
            Some("Name")
        );
        assert!(validate_name("").is_none());
        assert!(validate_name(&"\u{1f600}".repeat(100)).is_some());
        assert!(validate_name(&"\u{1f600}".repeat(101)).is_none());
        assert!(validate_name("a\u{7f}b").is_none());
    }

    #[test]
    fn failure_reasons_are_closed_safe_literals() {
        let reasons = [
            ProjectFailureReason::TimestampUnavailable,
            ProjectFailureReason::InsufficientSpace,
            ProjectFailureReason::StorageAlreadyExists,
            ProjectFailureReason::UnsafeStorage,
            ProjectFailureReason::StorageUnavailable,
            ProjectFailureReason::DatabaseInsertFailed,
            ProjectFailureReason::DirectoryCleanupFailed,
            ProjectFailureReason::CreateTaskFailed,
        ];
        assert_eq!(
            reasons.map(ProjectFailureReason::as_str),
            [
                "timestamp_unavailable",
                "insufficient_space",
                "storage_already_exists",
                "unsafe_storage",
                "storage_unavailable",
                "database_insert_failed",
                "directory_cleanup_failed",
                "create_task_failed",
            ]
        );
    }
}

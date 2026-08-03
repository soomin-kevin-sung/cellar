//! Upload session creation and status operations.

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use uuid::Uuid;

use crate::{
    app::RequestId,
    auth::OwnerIdentity,
    db::{Database, DbError, NewUpload, ProjectRow, UploadRow, UploadState},
    error::AppError,
    storage::{SafeFileName, Storage, StorageError},
};

/// A JSON decimal string backed by a SQLite-compatible nonnegative integer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecimalU64(pub u64);

impl Serialize for DecimalU64 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for DecimalU64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(de::Error::custom("expected nonnegative decimal digits"));
        }
        let value = value
            .parse::<u64>()
            .map_err(|_| de::Error::custom("decimal value is out of range"))?;
        if value > i64::MAX as u64 {
            return Err(de::Error::custom("decimal value is out of range"));
        }
        Ok(Self(value))
    }
}

pub type UploadRepositoryFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, DbError>> + Send + 'a>>;
pub type UploadStorageFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, StorageError>> + Send + 'a>>;

/// Database operations needed by upload session endpoints.
pub trait UploadRepository: Send + Sync {
    fn get_project<'a>(
        &'a self,
        project_id: Uuid,
    ) -> UploadRepositoryFuture<'a, Option<ProjectRow>>;
    fn create_upload<'a>(&'a self, upload: NewUpload) -> UploadRepositoryFuture<'a, UploadRow>;
    fn get_upload<'a>(&'a self, upload_id: Uuid) -> UploadRepositoryFuture<'a, Option<UploadRow>>;
}

impl UploadRepository for Database {
    fn get_project<'a>(
        &'a self,
        project_id: Uuid,
    ) -> UploadRepositoryFuture<'a, Option<ProjectRow>> {
        Box::pin(async move { self.get_project(project_id).await })
    }

    fn create_upload<'a>(&'a self, upload: NewUpload) -> UploadRepositoryFuture<'a, UploadRow> {
        Box::pin(async move { self.create_upload(upload).await })
    }

    fn get_upload<'a>(&'a self, upload_id: Uuid) -> UploadRepositoryFuture<'a, Option<UploadRow>> {
        Box::pin(async move { self.get_upload(upload_id).await })
    }
}

/// Filesystem operations needed to create upload sessions.
pub trait UploadStorage: Send + Sync {
    fn destination_exists<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, bool>;
    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()>;
    fn remove_empty_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()>;
}

impl UploadStorage for Storage {
    fn destination_exists<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, bool> {
        Box::pin(async move { self.destination_exists(project_id, file_name).await })
    }

    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.create_staging(upload_id).await })
    }

    fn remove_empty_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.remove_empty_staging(upload_id).await })
    }
}

#[derive(Clone)]
pub struct UploadService {
    repository: Arc<dyn UploadRepository>,
    storage: Arc<dyn UploadStorage>,
}

impl UploadService {
    pub fn new(repository: Arc<dyn UploadRepository>, storage: Arc<dyn UploadStorage>) -> Self {
        Self {
            repository,
            storage,
        }
    }

    async fn create(
        &self,
        project_id: Uuid,
        file_name: String,
        total_size: u64,
        request_id: &RequestId,
    ) -> Result<UploadRow, UploadServiceError> {
        let file_name =
            SafeFileName::parse(file_name).map_err(|_| UploadServiceError::InvalidRequest)?;
        match self.repository.get_project(project_id).await {
            Ok(Some(_)) => {}
            Ok(None) => return Err(UploadServiceError::ProjectNotFound),
            Err(_) => {
                record_failure(
                    UploadFailureReason::ProjectLookupFailed,
                    request_id,
                    None,
                    project_id,
                );
                return Err(UploadServiceError::CreateFailed);
            }
        }
        match self
            .storage
            .destination_exists(project_id, &file_name)
            .await
        {
            Ok(false) => {}
            Ok(true) => return Err(UploadServiceError::DestinationExists),
            Err(error) => {
                let result = storage_error(&error);
                record_failure(storage_failure_reason(&error), request_id, None, project_id);
                return Err(result);
            }
        }

        let upload_id = Uuid::now_v7();
        let upload = NewUpload::new(upload_id, project_id, file_name.as_str(), total_size)
            .map_err(|_| {
                record_failure(
                    UploadFailureReason::TimestampUnavailable,
                    request_id,
                    Some(upload_id),
                    project_id,
                );
                UploadServiceError::CreateFailed
            })?;
        let repository = self.repository.clone();
        let storage = self.storage.clone();
        let supervisor_request_id = request_id.clone();
        let operation_request_id = supervisor_request_id.clone();
        let supervisor = tokio::spawn(async move {
            let operation_repository = repository.clone();
            let operation_storage = storage.clone();
            let operation = tokio::spawn(async move {
                create_owned(
                    operation_repository,
                    operation_storage,
                    upload,
                    upload_id,
                    project_id,
                    operation_request_id,
                )
                .await
            });
            match operation.await {
                Ok(result) => result,
                Err(_) => {
                    run_reconciliation_task(
                        repository,
                        storage,
                        upload_id,
                        project_id,
                        &supervisor_request_id,
                    )
                    .await
                }
            }
        });
        match supervisor.await {
            Ok(result) => result,
            Err(_) => {
                record_failure(
                    UploadFailureReason::CreateSupervisorFailed,
                    request_id,
                    Some(upload_id),
                    project_id,
                );
                Err(UploadServiceError::CreateFailed)
            }
        }
    }

    async fn status(
        &self,
        upload_id: Uuid,
        request_id: &RequestId,
    ) -> Result<UploadRow, UploadServiceError> {
        match self.repository.get_upload(upload_id).await {
            Ok(Some(upload)) => Ok(upload),
            Ok(None) => Err(UploadServiceError::UploadNotFound),
            Err(_) => {
                tracing::warn!(
                    reason = "database_upload_lookup_failed",
                    request_id = %request_id,
                    upload_id = %upload_id,
                    "upload status lookup failed"
                );
                Err(UploadServiceError::StatusFailed)
            }
        }
    }
}

async fn run_reconciliation_task(
    repository: Arc<dyn UploadRepository>,
    storage: Arc<dyn UploadStorage>,
    upload_id: Uuid,
    project_id: Uuid,
    request_id: &RequestId,
) -> Result<UploadRow, UploadServiceError> {
    let reconciliation_request_id = request_id.clone();
    let reconciliation = tokio::spawn(async move {
        reconcile_create_task_failure(
            repository,
            storage,
            upload_id,
            project_id,
            &reconciliation_request_id,
        )
        .await
    });
    match reconciliation.await {
        Ok(result) => result,
        Err(_) => {
            record_failure(
                UploadFailureReason::ReconciliationTaskFailed,
                request_id,
                Some(upload_id),
                project_id,
            );
            Err(UploadServiceError::CreateFailed)
        }
    }
}

async fn create_owned(
    repository: Arc<dyn UploadRepository>,
    storage: Arc<dyn UploadStorage>,
    upload: NewUpload,
    upload_id: Uuid,
    project_id: Uuid,
    request_id: RequestId,
) -> Result<UploadRow, UploadServiceError> {
    if let Err(error) = storage.create_staging(upload_id).await {
        let result = storage_error(&error);
        record_failure(
            storage_failure_reason(&error),
            &request_id,
            Some(upload_id),
            project_id,
        );
        return Err(result);
    }

    match repository.create_upload(upload).await {
        Ok(upload) => Ok(upload),
        Err(_) => {
            compensate_empty_staging(
                storage,
                upload_id,
                project_id,
                &request_id,
                UploadFailureReason::DatabaseInsertFailed,
            )
            .await
        }
    }
}

async fn reconcile_create_task_failure(
    repository: Arc<dyn UploadRepository>,
    storage: Arc<dyn UploadStorage>,
    upload_id: Uuid,
    project_id: Uuid,
    request_id: &RequestId,
) -> Result<UploadRow, UploadServiceError> {
    match repository.get_upload(upload_id).await {
        Ok(Some(upload)) => {
            record_failure(
                UploadFailureReason::CreateTaskReconciled,
                request_id,
                Some(upload_id),
                project_id,
            );
            Ok(upload)
        }
        Ok(None) => {
            compensate_empty_staging(
                storage,
                upload_id,
                project_id,
                request_id,
                UploadFailureReason::CreateTaskFailed,
            )
            .await
        }
        Err(_) => {
            record_failure(
                UploadFailureReason::DatabaseReconciliationFailed,
                request_id,
                Some(upload_id),
                project_id,
            );
            Err(UploadServiceError::CreateFailed)
        }
    }
}

async fn compensate_empty_staging(
    storage: Arc<dyn UploadStorage>,
    upload_id: Uuid,
    project_id: Uuid,
    request_id: &RequestId,
    failure_reason: UploadFailureReason,
) -> Result<UploadRow, UploadServiceError> {
    match storage.remove_empty_staging(upload_id).await {
        Ok(()) => {
            record_failure(failure_reason, request_id, Some(upload_id), project_id);
            Err(UploadServiceError::CreateFailed)
        }
        Err(_) => {
            record_failure(
                UploadFailureReason::StagingCleanupFailed,
                request_id,
                Some(upload_id),
                project_id,
            );
            Err(UploadServiceError::CleanupFailed)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UploadServiceError {
    InvalidRequest,
    ProjectNotFound,
    UploadNotFound,
    DestinationExists,
    InsufficientStorage,
    CreateFailed,
    StatusFailed,
    CleanupFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UploadFailureReason {
    ProjectLookupFailed,
    TimestampUnavailable,
    InsufficientSpace,
    StorageConflict,
    UnsafeStorage,
    StorageUnavailable,
    DatabaseInsertFailed,
    StagingCleanupFailed,
    CreateTaskFailed,
    CreateTaskReconciled,
    DatabaseReconciliationFailed,
    ReconciliationTaskFailed,
    CreateSupervisorFailed,
}

impl UploadFailureReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ProjectLookupFailed => "database_project_lookup_failed",
            Self::TimestampUnavailable => "timestamp_unavailable",
            Self::InsufficientSpace => "insufficient_space",
            Self::StorageConflict => "storage_conflict",
            Self::UnsafeStorage => "unsafe_storage",
            Self::StorageUnavailable => "storage_unavailable",
            Self::DatabaseInsertFailed => "database_insert_failed",
            Self::StagingCleanupFailed => "staging_cleanup_failed",
            Self::CreateTaskFailed => "create_task_failed",
            Self::CreateTaskReconciled => "create_task_reconciled",
            Self::DatabaseReconciliationFailed => "database_reconciliation_failed",
            Self::ReconciliationTaskFailed => "reconciliation_task_failed",
            Self::CreateSupervisorFailed => "create_supervisor_failed",
        }
    }
}

fn storage_error(error: &StorageError) -> UploadServiceError {
    match error {
        StorageError::InsufficientSpace => UploadServiceError::InsufficientStorage,
        _ => UploadServiceError::CreateFailed,
    }
}

fn storage_failure_reason(error: &StorageError) -> UploadFailureReason {
    match error {
        StorageError::InsufficientSpace => UploadFailureReason::InsufficientSpace,
        StorageError::AlreadyExists => UploadFailureReason::StorageConflict,
        StorageError::InvalidRoot(_)
        | StorageError::UnsafeManagedEntry
        | StorageError::UnsafeEntry
        | StorageError::NonEmptyStaging => UploadFailureReason::UnsafeStorage,
        StorageError::NotFound
        | StorageError::OffsetMismatch { .. }
        | StorageError::ProjectCleanupFailed { .. }
        | StorageError::AmbiguousCleanup { .. }
        | StorageError::Io { .. } => UploadFailureReason::StorageUnavailable,
    }
}

fn record_failure(
    reason: UploadFailureReason,
    request_id: &RequestId,
    upload_id: Option<Uuid>,
    project_id: Uuid,
) {
    tracing::warn!(
        reason = reason.as_str(),
        request_id = %request_id,
        upload_id = upload_id.map(|id| id.to_string()).as_deref().unwrap_or("not_assigned"),
        project_id = %project_id,
        "upload session creation failed"
    );
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateUploadRequest {
    file_name: String,
    total_size: DecimalU64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadResponse {
    id: Uuid,
    project_id: Uuid,
    file_name: String,
    total_size: DecimalU64,
    committed_offset: DecimalU64,
    state: &'static str,
}

impl From<UploadRow> for UploadResponse {
    fn from(upload: UploadRow) -> Self {
        let state = match upload.state() {
            UploadState::Active => "active",
            UploadState::Finalizing => "finalizing",
            UploadState::Complete => "complete",
            UploadState::Failed => "failed",
        };
        Self {
            id: upload.id(),
            project_id: upload.project_id(),
            file_name: upload.file_name().to_owned(),
            total_size: DecimalU64(upload.total_size()),
            committed_offset: DecimalU64(upload.committed_offset()),
            state,
        }
    }
}

pub fn upload_router(service: UploadService) -> Router {
    Router::new()
        .route("/api/v1/projects/{project_id}/uploads", post(create_upload))
        .route("/api/v1/uploads/{upload_id}", get(upload_status))
        .with_state(service)
}

async fn create_upload(
    State(service): State<UploadService>,
    Extension(request_id): Extension<RequestId>,
    Extension(_owner): Extension<OwnerIdentity>,
    Path(project_id): Path<String>,
    payload: Result<Json<CreateUploadRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<UploadResponse>), AppError> {
    let project_id =
        parse_canonical_uuid(&project_id).ok_or_else(|| invalid_request(request_id.clone()))?;
    let Json(payload) = payload.map_err(|_| invalid_request(request_id.clone()))?;
    let upload = service
        .create(
            project_id,
            payload.file_name,
            payload.total_size.0,
            &request_id,
        )
        .await
        .map_err(|error| map_service_error(error, request_id.clone()))?;
    Ok((StatusCode::CREATED, Json(upload.into())))
}

async fn upload_status(
    State(service): State<UploadService>,
    Extension(request_id): Extension<RequestId>,
    Extension(_owner): Extension<OwnerIdentity>,
    Path(upload_id): Path<String>,
) -> Result<Json<UploadResponse>, AppError> {
    let upload_id =
        parse_canonical_uuid(&upload_id).ok_or_else(|| invalid_request(request_id.clone()))?;
    service
        .status(upload_id, &request_id)
        .await
        .map(|upload| Json(upload.into()))
        .map_err(|error| map_service_error(error, request_id))
}

fn map_service_error(error: UploadServiceError, request_id: RequestId) -> AppError {
    match error {
        UploadServiceError::InvalidRequest => invalid_request(request_id),
        UploadServiceError::ProjectNotFound => AppError::not_found(
            request_id,
            "project_not_found",
            "The project was not found.",
        ),
        UploadServiceError::UploadNotFound => {
            AppError::not_found(request_id, "upload_not_found", "The upload was not found.")
        }
        UploadServiceError::DestinationExists => AppError::conflict(
            request_id,
            "destination_exists",
            "A file with that name already exists.",
            None,
        ),
        UploadServiceError::InsufficientStorage => AppError::insufficient_storage(request_id),
        UploadServiceError::CreateFailed => AppError::service_unavailable(
            request_id,
            "upload_create_failed",
            "Upload creation is temporarily unavailable.",
        ),
        UploadServiceError::StatusFailed => AppError::service_unavailable(
            request_id,
            "upload_status_failed",
            "Upload status is temporarily unavailable.",
        ),
        UploadServiceError::CleanupFailed => AppError::service_unavailable(
            request_id,
            "upload_cleanup_failed",
            "Upload creation could not be completed safely.",
        ),
    }
}

fn invalid_request(request_id: RequestId) -> AppError {
    AppError::bad_request(request_id, "invalid_request", "The request is invalid.")
}

fn parse_canonical_uuid(value: &str) -> Option<Uuid> {
    let id = Uuid::parse_str(value).ok()?;
    (id.to_string() == value).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::UploadFailureReason;

    #[test]
    fn failure_reasons_are_closed_safe_literals() {
        assert_eq!(
            [
                UploadFailureReason::ProjectLookupFailed,
                UploadFailureReason::TimestampUnavailable,
                UploadFailureReason::InsufficientSpace,
                UploadFailureReason::StorageConflict,
                UploadFailureReason::UnsafeStorage,
                UploadFailureReason::StorageUnavailable,
                UploadFailureReason::DatabaseInsertFailed,
                UploadFailureReason::StagingCleanupFailed,
                UploadFailureReason::CreateTaskFailed,
                UploadFailureReason::CreateTaskReconciled,
                UploadFailureReason::DatabaseReconciliationFailed,
                UploadFailureReason::ReconciliationTaskFailed,
                UploadFailureReason::CreateSupervisorFailed,
            ]
            .map(UploadFailureReason::as_str),
            [
                "database_project_lookup_failed",
                "timestamp_unavailable",
                "insufficient_space",
                "storage_conflict",
                "unsafe_storage",
                "storage_unavailable",
                "database_insert_failed",
                "staging_cleanup_failed",
                "create_task_failed",
                "create_task_reconciled",
                "database_reconciliation_failed",
                "reconciliation_task_failed",
                "create_supervisor_failed",
            ]
        );
    }
}

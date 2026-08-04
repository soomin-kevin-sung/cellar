//! Single-request project file uploads.

use std::{future::Future, io, pin::Pin, sync::Arc};

use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{
        Path, Query, State,
        rejection::{PathRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode, header},
    routing::post,
};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncRead;
use tokio_util::io::StreamReader;
use uuid::Uuid;

use crate::{
    app::RequestId,
    auth::OwnerIdentity,
    db::{Database, DbError, ProjectRow},
    error::AppError,
    storage::{SafeFileName, Storage, StorageError},
};

pub type UploadFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type UploadStorageFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, StorageError>> + Send + 'a>>;
pub type UploadBodyReader = Pin<Box<dyn AsyncRead + Send>>;

/// Project lookup needed before an upload may touch storage.
pub trait UploadRepository: Send + Sync {
    fn get_project<'a>(
        &'a self,
        project_id: Uuid,
    ) -> UploadFuture<'a, Result<Option<ProjectRow>, DbError>>;
}

impl UploadRepository for Database {
    fn get_project<'a>(
        &'a self,
        project_id: Uuid,
    ) -> UploadFuture<'a, Result<Option<ProjectRow>, DbError>> {
        Box::pin(async move { self.get_project(project_id).await })
    }
}

/// Exact staging operations needed to publish one request body.
pub trait UploadStorage: Send + Sync {
    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()>;

    fn write_upload<'a>(
        &'a self,
        upload_id: Uuid,
        reader: UploadBodyReader,
    ) -> UploadStorageFuture<'a, u64>;

    fn sync_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()>;

    fn finalize_no_replace<'a>(
        &'a self,
        upload_id: Uuid,
        project_id: Uuid,
        name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, ()>;

    fn remove_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()>;
}

impl UploadStorage for Storage {
    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.create_staging(upload_id).await })
    }

    fn write_upload<'a>(
        &'a self,
        upload_id: Uuid,
        reader: UploadBodyReader,
    ) -> UploadStorageFuture<'a, u64> {
        Box::pin(async move { self.write_chunk(upload_id, 0, reader).await })
    }

    fn sync_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.sync_staging(upload_id).await })
    }

    fn finalize_no_replace<'a>(
        &'a self,
        upload_id: Uuid,
        project_id: Uuid,
        name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.finalize_no_replace(upload_id, project_id, name).await })
    }

    fn remove_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.remove_staging(upload_id).await })
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

    /// Temporary startup compatibility until the following migration task replaces
    /// session recovery with unconditional staging cleanup.
    pub async fn recover_uploads(&self) -> Result<(), UploadRecoveryError> {
        Ok(())
    }

    async fn upload(
        &self,
        project_id: Uuid,
        file_name: String,
        reader: UploadBodyReader,
        request_id: &RequestId,
    ) -> Result<UploadResponse, UploadServiceError> {
        let name =
            SafeFileName::parse(file_name).map_err(|_| UploadServiceError::InvalidRequest)?;
        match self.repository.get_project(project_id).await {
            Ok(Some(_)) => {}
            Ok(None) => return Err(UploadServiceError::ProjectNotFound),
            Err(_) => {
                record_failure("project_lookup_failed", request_id, project_id);
                return Err(UploadServiceError::Unavailable);
            }
        }

        let storage = self.storage.clone();
        let request_id = request_id.clone();
        let operation = tokio::spawn(async move {
            run_owned_upload(storage, project_id, name, reader, request_id).await
        });
        match operation.await {
            Ok(result) => result,
            Err(_) => Err(UploadServiceError::Unavailable),
        }
    }
}

async fn run_owned_upload(
    storage: Arc<dyn UploadStorage>,
    project_id: Uuid,
    name: SafeFileName,
    reader: UploadBodyReader,
    request_id: RequestId,
) -> Result<UploadResponse, UploadServiceError> {
    let upload_id = Uuid::now_v7();
    let mut cleanup = StagingCleanup::new(storage.clone(), upload_id);
    if let Err(error) = storage.create_staging(upload_id).await {
        cleanup.disarm();
        return Err(map_internal_storage_error(&error));
    }

    let size = match storage.write_upload(upload_id, reader).await {
        Ok(size) => size,
        Err(error) => {
            let mapped = map_write_error(&error);
            record_storage_failure(&error, &request_id, project_id);
            return Err(cleanup_result(&mut cleanup, mapped, &request_id, project_id).await);
        }
    };
    if size == 0 {
        return Err(cleanup_result(
            &mut cleanup,
            UploadServiceError::InvalidRequest,
            &request_id,
            project_id,
        )
        .await);
    }

    if let Err(error) = storage.sync_staging(upload_id).await {
        let mapped = map_internal_storage_error(&error);
        record_storage_failure(&error, &request_id, project_id);
        return Err(cleanup_result(&mut cleanup, mapped, &request_id, project_id).await);
    }

    let final_storage = storage.clone();
    let final_name = name.clone();
    let cleanup_gate = cleanup.gate();
    let finalization = tokio::spawn(async move {
        let _guard = cleanup_gate.lock().await;
        final_storage
            .finalize_no_replace(upload_id, project_id, &final_name)
            .await
    });
    let finalization = match finalization.await {
        Ok(result) => result,
        Err(_) => {
            record_failure("finalization_task_failed", &request_id, project_id);
            return Err(cleanup_result(
                &mut cleanup,
                UploadServiceError::Unavailable,
                &request_id,
                project_id,
            )
            .await);
        }
    };
    if let Err(error) = finalization {
        let mapped = map_finalization_error(&error);
        record_storage_failure(&error, &request_id, project_id);
        return Err(cleanup_result(&mut cleanup, mapped, &request_id, project_id).await);
    }

    cleanup.disarm();
    Ok(UploadResponse {
        name: name.as_str().to_owned(),
        size: DecimalU64(size),
    })
}

async fn cleanup_result(
    cleanup: &mut StagingCleanup,
    original: UploadServiceError,
    request_id: &RequestId,
    project_id: Uuid,
) -> UploadServiceError {
    match cleanup.cleanup().await {
        Ok(()) => original,
        Err(_) => {
            record_failure("staging_cleanup_failed", request_id, project_id);
            UploadServiceError::Unavailable
        }
    }
}

#[derive(Debug)]
pub struct UploadRecoveryError;

impl std::fmt::Display for UploadRecoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("upload recovery failed")
    }
}

impl std::error::Error for UploadRecoveryError {}

struct StagingCleanup {
    storage: Arc<dyn UploadStorage>,
    upload_id: Uuid,
    cleanup_gate: Arc<tokio::sync::Mutex<()>>,
    armed: bool,
}

impl StagingCleanup {
    fn new(storage: Arc<dyn UploadStorage>, upload_id: Uuid) -> Self {
        Self {
            storage,
            upload_id,
            cleanup_gate: Arc::new(tokio::sync::Mutex::new(())),
            armed: true,
        }
    }

    async fn cleanup(&mut self) -> Result<(), StorageError> {
        let cleanup_gate = self.cleanup_gate.clone();
        let _guard = cleanup_gate.lock().await;
        match self.storage.remove_staging(self.upload_id).await {
            Ok(()) => {
                self.disarm();
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn gate(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.cleanup_gate.clone()
    }
}

impl Drop for StagingCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let storage = self.storage.clone();
        let upload_id = self.upload_id;
        let cleanup_gate = self.cleanup_gate.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _guard = cleanup_gate.lock().await;
                if storage.remove_staging(upload_id).await.is_err() {
                    tracing::warn!(upload_id = %upload_id, "staging cleanup retry failed");
                }
            });
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadServiceError {
    InvalidRequest,
    ProjectNotFound,
    Conflict,
    InsufficientStorage,
    Unavailable,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UploadQuery {
    file_name: String,
}

#[derive(Serialize)]
struct UploadResponse {
    name: String,
    size: DecimalU64,
}

/// Serializes response byte counts as decimal strings to preserve full `u64` precision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecimalU64(pub u64);

impl Serialize for DecimalU64 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

pub fn upload_router(service: UploadService) -> Router {
    Router::new()
        .route("/api/v1/projects/{project_id}/uploads", post(create_upload))
        .with_state(service)
}

async fn create_upload(
    State(service): State<UploadService>,
    Extension(request_id): Extension<RequestId>,
    Extension(_owner): Extension<OwnerIdentity>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<UploadQuery>, QueryRejection>,
    headers: HeaderMap,
    body: Body,
) -> Result<(StatusCode, Json<UploadResponse>), AppError> {
    let Path(project_id) = path.map_err(|_| invalid_request(request_id.clone()))?;
    let project_id =
        parse_canonical_uuid(&project_id).ok_or_else(|| invalid_request(request_id.clone()))?;
    let Query(query) = query.map_err(|_| invalid_request(request_id.clone()))?;
    require_octet_stream(&headers).map_err(|()| invalid_request(request_id.clone()))?;

    let stream = body
        .into_data_stream()
        .map_err(|error| io::Error::other(error.to_string()));
    let reader: UploadBodyReader = Box::pin(StreamReader::new(stream));
    let response = service
        .upload(project_id, query.file_name, reader, &request_id)
        .await
        .map_err(|error| upload_error(error, request_id))?;
    Ok((StatusCode::CREATED, Json(response)))
}

fn require_octet_stream(headers: &HeaderMap) -> Result<(), ()> {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let Some(value) = values.next() else {
        return Err(());
    };
    if values.next().is_some() || value.as_bytes() != b"application/octet-stream" {
        return Err(());
    }
    Ok(())
}

fn parse_canonical_uuid(value: &str) -> Option<Uuid> {
    let id = Uuid::parse_str(value).ok()?;
    (id.hyphenated().to_string() == value).then_some(id)
}

fn map_write_error(error: &StorageError) -> UploadServiceError {
    match error {
        StorageError::InvalidBody => UploadServiceError::InvalidRequest,
        StorageError::InsufficientSpace => UploadServiceError::InsufficientStorage,
        StorageError::InvalidRoot(_)
        | StorageError::AlreadyExists
        | StorageError::NotFound
        | StorageError::UnsafeManagedEntry
        | StorageError::UnsafeEntry
        | StorageError::NonEmptyStaging
        | StorageError::OffsetMismatch { .. }
        | StorageError::ProjectCleanupFailed { .. }
        | StorageError::AmbiguousCleanup { .. }
        | StorageError::Io { .. } => UploadServiceError::Unavailable,
    }
}

fn map_internal_storage_error(error: &StorageError) -> UploadServiceError {
    if matches!(error, StorageError::InsufficientSpace) {
        UploadServiceError::InsufficientStorage
    } else {
        UploadServiceError::Unavailable
    }
}

fn map_finalization_error(error: &StorageError) -> UploadServiceError {
    match error {
        StorageError::AlreadyExists => UploadServiceError::Conflict,
        StorageError::InsufficientSpace => UploadServiceError::InsufficientStorage,
        _ => UploadServiceError::Unavailable,
    }
}

fn upload_error(error: UploadServiceError, request_id: RequestId) -> AppError {
    match error {
        UploadServiceError::InvalidRequest => invalid_request(request_id),
        UploadServiceError::ProjectNotFound => AppError::not_found(
            request_id,
            "project_not_found",
            "The project was not found.",
        ),
        UploadServiceError::Conflict => AppError::conflict(
            request_id,
            "file_conflict",
            "A file with that name already exists.",
            None,
        ),
        UploadServiceError::InsufficientStorage => AppError::insufficient_storage(request_id),
        UploadServiceError::Unavailable => AppError::service_unavailable(
            request_id,
            "upload_unavailable",
            "Uploads are temporarily unavailable.",
        ),
    }
}

fn invalid_request(request_id: RequestId) -> AppError {
    AppError::bad_request(request_id, "invalid_request", "The request is invalid.")
}

fn record_storage_failure(error: &StorageError, request_id: &RequestId, project_id: Uuid) {
    let reason = match error {
        StorageError::InvalidBody => "invalid_body",
        StorageError::AlreadyExists => "destination_exists",
        StorageError::InsufficientSpace => "insufficient_space",
        StorageError::InvalidRoot(_) => "invalid_storage_root",
        StorageError::NotFound => "storage_entry_missing",
        StorageError::UnsafeManagedEntry | StorageError::UnsafeEntry => "unsafe_storage_entry",
        StorageError::NonEmptyStaging => "non_empty_staging",
        StorageError::OffsetMismatch { .. } => "staging_offset_mismatch",
        StorageError::ProjectCleanupFailed { .. } => "project_cleanup_failed",
        StorageError::AmbiguousCleanup { .. } => "ambiguous_storage_cleanup",
        StorageError::Io { .. } => "storage_io_failed",
    };
    record_failure(reason, request_id, project_id);
}

fn record_failure(reason: &'static str, request_id: &RequestId, project_id: Uuid) {
    tracing::warn!(
        reason,
        request_id = %request_id,
        project_id = %project_id,
        "upload failed"
    );
}

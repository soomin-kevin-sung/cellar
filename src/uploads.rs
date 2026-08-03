//! Upload session creation and status operations.

use std::{
    collections::HashMap,
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex as StdMutex, Weak},
};

use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use futures_util::TryStreamExt;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::io::StreamReader;
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
pub type UploadBodyReader = Pin<Box<dyn AsyncRead + Send>>;

/// Database operations needed by upload session endpoints.
pub trait UploadRepository: Send + Sync {
    fn get_project<'a>(
        &'a self,
        project_id: Uuid,
    ) -> UploadRepositoryFuture<'a, Option<ProjectRow>>;
    fn create_upload<'a>(&'a self, upload: NewUpload) -> UploadRepositoryFuture<'a, UploadRow>;
    fn get_upload<'a>(&'a self, upload_id: Uuid) -> UploadRepositoryFuture<'a, Option<UploadRow>>;
    fn advance_upload_offset<'a>(
        &'a self,
        _upload_id: Uuid,
        _expected: u64,
        _next: u64,
    ) -> UploadRepositoryFuture<'a, bool> {
        Box::pin(async { Err(DbError::InvalidTransition) })
    }
    fn mark_upload_failed<'a>(
        &'a self,
        _upload_id: Uuid,
        _reason: &'a str,
    ) -> UploadRepositoryFuture<'a, ()> {
        Box::pin(async { Err(DbError::InvalidTransition) })
    }
    fn mark_upload_finalizing<'a>(
        &'a self,
        _upload_id: Uuid,
        _expected: u64,
    ) -> UploadRepositoryFuture<'a, bool> {
        Box::pin(async { Err(DbError::InvalidTransition) })
    }
    fn mark_upload_complete<'a>(&'a self, _upload_id: Uuid) -> UploadRepositoryFuture<'a, ()> {
        Box::pin(async { Err(DbError::InvalidTransition) })
    }
    fn recoverable_uploads(&self) -> UploadRepositoryFuture<'_, Vec<UploadRow>> {
        Box::pin(async { Err(DbError::InvalidTransition) })
    }
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

    fn advance_upload_offset<'a>(
        &'a self,
        upload_id: Uuid,
        expected: u64,
        next: u64,
    ) -> UploadRepositoryFuture<'a, bool> {
        Box::pin(async move { self.advance_offset(upload_id, expected, next).await })
    }

    fn mark_upload_failed<'a>(
        &'a self,
        upload_id: Uuid,
        reason: &'a str,
    ) -> UploadRepositoryFuture<'a, ()> {
        Box::pin(async move { self.mark_failed(upload_id, reason).await })
    }

    fn mark_upload_finalizing<'a>(
        &'a self,
        upload_id: Uuid,
        expected: u64,
    ) -> UploadRepositoryFuture<'a, bool> {
        Box::pin(async move { self.mark_finalizing(upload_id, expected).await })
    }

    fn mark_upload_complete<'a>(&'a self, upload_id: Uuid) -> UploadRepositoryFuture<'a, ()> {
        Box::pin(async move { self.mark_complete(upload_id).await })
    }

    fn recoverable_uploads(&self) -> UploadRepositoryFuture<'_, Vec<UploadRow>> {
        Box::pin(async move { self.recoverable_uploads().await })
    }
}

/// Filesystem operations needed by upload session endpoints and startup recovery.
pub trait UploadStorage: Send + Sync {
    fn destination_exists<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, bool>;
    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()>;
    fn remove_empty_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()>;
    fn write_staging_chunk<'a>(
        &'a self,
        _upload_id: Uuid,
        _offset: u64,
        _reader: UploadBodyReader,
    ) -> UploadStorageFuture<'a, u64> {
        Box::pin(async {
            Err(StorageError::Io {
                source: io::Error::other("chunk writes are unavailable"),
            })
        })
    }
    fn truncate_staging<'a>(&'a self, _upload_id: Uuid, _len: u64) -> UploadStorageFuture<'a, ()> {
        Box::pin(async {
            Err(StorageError::Io {
                source: io::Error::other("staging rollback is unavailable"),
            })
        })
    }
    fn staging_len<'a>(&'a self, _upload_id: Uuid) -> UploadStorageFuture<'a, Option<u64>> {
        Box::pin(async {
            Err(StorageError::Io {
                source: io::Error::other("staging inspection is unavailable"),
            })
        })
    }
    fn sync_staging<'a>(&'a self, _upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async {
            Err(StorageError::Io {
                source: io::Error::other("staging sync is unavailable"),
            })
        })
    }
    fn finalize_no_replace<'a>(
        &'a self,
        _upload_id: Uuid,
        _project_id: Uuid,
        _file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, ()> {
        Box::pin(async {
            Err(StorageError::Io {
                source: io::Error::other("upload finalization is unavailable"),
            })
        })
    }
    fn final_file_len<'a>(
        &'a self,
        _project_id: Uuid,
        _file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, Option<u64>> {
        Box::pin(async {
            Err(StorageError::Io {
                source: io::Error::other("final file inspection is unavailable"),
            })
        })
    }
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

    fn write_staging_chunk<'a>(
        &'a self,
        upload_id: Uuid,
        offset: u64,
        reader: UploadBodyReader,
    ) -> UploadStorageFuture<'a, u64> {
        Box::pin(async move { self.write_chunk(upload_id, offset, reader).await })
    }

    fn truncate_staging<'a>(&'a self, upload_id: Uuid, len: u64) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.truncate_staging(upload_id, len).await })
    }

    fn staging_len<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, Option<u64>> {
        Box::pin(async move { self.staging_len(upload_id).await })
    }

    fn sync_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.sync_staging(upload_id).await })
    }

    fn finalize_no_replace<'a>(
        &'a self,
        upload_id: Uuid,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move {
            self.finalize_no_replace(upload_id, project_id, file_name)
                .await
        })
    }

    fn final_file_len<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, Option<u64>> {
        Box::pin(async move { self.final_file_len(project_id, file_name).await })
    }
}

type UploadLock = tokio::sync::Mutex<()>;
type UploadLockRegistry = StdMutex<HashMap<Uuid, Weak<UploadLock>>>;

#[derive(Clone)]
pub struct UploadService {
    repository: Arc<dyn UploadRepository>,
    storage: Arc<dyn UploadStorage>,
    upload_locks: Arc<UploadLockRegistry>,
}

impl UploadService {
    pub fn new(repository: Arc<dyn UploadRepository>, storage: Arc<dyn UploadStorage>) -> Self {
        Self {
            repository,
            storage,
            upload_locks: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    fn lock_for(&self, upload_id: Uuid) -> Arc<UploadLock> {
        let mut locks = self
            .upload_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&upload_id).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(UploadLock::new(()));
        locks.insert(upload_id, Arc::downgrade(&lock));
        lock
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

    async fn complete(
        &self,
        upload_id: Uuid,
        request_id: &RequestId,
    ) -> Result<UploadRow, UploadServiceError> {
        let lock = self.lock_for(upload_id);
        let _guard = lock.lock().await;
        let upload = self
            .repository
            .get_upload(upload_id)
            .await
            .map_err(|_| {
                record_completion_failure(
                    CompletionFailureReason::DatabaseLookupFailed,
                    request_id,
                    upload_id,
                    None,
                );
                UploadServiceError::FinalizeFailed
            })?
            .ok_or(UploadServiceError::UploadNotFound)?;
        if upload.state() == UploadState::Complete {
            return Ok(upload);
        }
        if upload.state() != UploadState::Active {
            return Err(UploadServiceError::UploadInactive);
        }
        if upload.committed_offset() != upload.total_size() {
            return Err(UploadServiceError::UploadIncomplete);
        }
        let project_id = upload.project_id();
        let total_size = upload.total_size();
        let file_name = SafeFileName::parse(upload.file_name()).map_err(|_| {
            record_completion_failure(
                CompletionFailureReason::InvalidStoredFileName,
                request_id,
                upload_id,
                Some(project_id),
            );
            UploadServiceError::FinalizeFailed
        })?;

        match self.storage.staging_len(upload_id).await {
            Ok(Some(len)) if len == total_size => {}
            Ok(_) | Err(StorageError::UnsafeEntry) => {
                if self
                    .repository
                    .mark_upload_failed(upload_id, "invalid staging evidence")
                    .await
                    .is_err()
                {
                    record_completion_failure(
                        CompletionFailureReason::FailureMarkFailed,
                        request_id,
                        upload_id,
                        Some(project_id),
                    );
                    return Err(UploadServiceError::FinalizeFailed);
                }
                record_completion_failure(
                    CompletionFailureReason::StagingInvalid,
                    request_id,
                    upload_id,
                    Some(project_id),
                );
                return Err(UploadServiceError::InvalidStaging);
            }
            Err(error) => {
                record_completion_failure(
                    completion_storage_failure(&error),
                    request_id,
                    upload_id,
                    Some(project_id),
                );
                return Err(finalization_storage_error(&error));
            }
        }
        match self.storage.final_file_len(project_id, &file_name).await {
            Ok(None) => {}
            Ok(Some(_)) | Err(StorageError::UnsafeEntry) => {
                if self
                    .repository
                    .mark_upload_failed(upload_id, "final destination conflict")
                    .await
                    .is_err()
                {
                    record_completion_failure(
                        CompletionFailureReason::FailureMarkFailed,
                        request_id,
                        upload_id,
                        Some(project_id),
                    );
                    return Err(UploadServiceError::FinalizeFailed);
                }
                record_completion_failure(
                    CompletionFailureReason::DestinationConflict,
                    request_id,
                    upload_id,
                    Some(project_id),
                );
                return Err(UploadServiceError::DestinationExists);
            }
            Err(error) => {
                record_completion_failure(
                    completion_storage_failure(&error),
                    request_id,
                    upload_id,
                    Some(project_id),
                );
                return Err(finalization_storage_error(&error));
            }
        }

        match self
            .repository
            .mark_upload_finalizing(upload_id, total_size)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return self.classify_finalizing_race(upload_id, request_id).await;
            }
            Err(_) => {
                record_completion_failure(
                    CompletionFailureReason::MarkFinalizingFailed,
                    request_id,
                    upload_id,
                    Some(project_id),
                );
                return Err(UploadServiceError::FinalizeFailed);
            }
        }

        if let Err(error) = self.storage.sync_staging(upload_id).await {
            if matches!(error, StorageError::UnsafeEntry) {
                if self
                    .repository
                    .mark_upload_failed(upload_id, "unsafe staging after preflight")
                    .await
                    .is_err()
                {
                    record_completion_failure(
                        CompletionFailureReason::FailureMarkFailed,
                        request_id,
                        upload_id,
                        Some(project_id),
                    );
                    return Err(UploadServiceError::FinalizeFailed);
                }
                record_completion_failure(
                    CompletionFailureReason::StagingInvalid,
                    request_id,
                    upload_id,
                    Some(project_id),
                );
                return Err(UploadServiceError::InvalidStaging);
            }
            record_completion_failure(
                completion_storage_failure(&error),
                request_id,
                upload_id,
                Some(project_id),
            );
            return Err(finalization_storage_error(&error));
        }
        if let Err(error) = self
            .storage
            .finalize_no_replace(upload_id, project_id, &file_name)
            .await
        {
            if matches!(
                error,
                StorageError::AlreadyExists | StorageError::UnsafeEntry
            ) {
                if self
                    .repository
                    .mark_upload_failed(upload_id, "final destination conflict")
                    .await
                    .is_err()
                {
                    record_completion_failure(
                        CompletionFailureReason::FailureMarkFailed,
                        request_id,
                        upload_id,
                        Some(project_id),
                    );
                    return Err(UploadServiceError::FinalizeFailed);
                }
                record_completion_failure(
                    CompletionFailureReason::DestinationConflict,
                    request_id,
                    upload_id,
                    Some(project_id),
                );
                return Err(UploadServiceError::DestinationExists);
            }
            record_completion_failure(
                completion_storage_failure(&error),
                request_id,
                upload_id,
                Some(project_id),
            );
            return Err(finalization_storage_error(&error));
        }
        match self.storage.final_file_len(project_id, &file_name).await {
            Ok(Some(len)) if len == total_size => {}
            Ok(_) => {
                record_completion_failure(
                    CompletionFailureReason::FinalSizeMismatch,
                    request_id,
                    upload_id,
                    Some(project_id),
                );
                return Err(UploadServiceError::FinalizeFailed);
            }
            Err(error) => {
                if matches!(error, StorageError::UnsafeEntry) {
                    if self
                        .repository
                        .mark_upload_failed(upload_id, "unsafe destination after publication")
                        .await
                        .is_err()
                    {
                        record_completion_failure(
                            CompletionFailureReason::FailureMarkFailed,
                            request_id,
                            upload_id,
                            Some(project_id),
                        );
                        return Err(UploadServiceError::FinalizeFailed);
                    }
                    record_completion_failure(
                        CompletionFailureReason::DestinationConflict,
                        request_id,
                        upload_id,
                        Some(project_id),
                    );
                    return Err(UploadServiceError::DestinationExists);
                }
                record_completion_failure(
                    completion_storage_failure(&error),
                    request_id,
                    upload_id,
                    Some(project_id),
                );
                return Err(finalization_storage_error(&error));
            }
        }
        if self
            .repository
            .mark_upload_complete(upload_id)
            .await
            .is_err()
        {
            record_completion_failure(
                CompletionFailureReason::CompleteTransitionFailed,
                request_id,
                upload_id,
                Some(project_id),
            );
            return Err(UploadServiceError::FinalizeFailed);
        }
        match self.repository.get_upload(upload_id).await {
            Ok(Some(upload)) if upload.state() == UploadState::Complete => Ok(upload),
            _ => {
                record_completion_failure(
                    CompletionFailureReason::CompleteReloadFailed,
                    request_id,
                    upload_id,
                    Some(project_id),
                );
                Err(UploadServiceError::FinalizeFailed)
            }
        }
    }

    async fn classify_finalizing_race(
        &self,
        upload_id: Uuid,
        request_id: &RequestId,
    ) -> Result<UploadRow, UploadServiceError> {
        match self.repository.get_upload(upload_id).await {
            Ok(Some(upload)) if upload.state() == UploadState::Complete => Ok(upload),
            Ok(Some(upload)) if upload.state() == UploadState::Active => {
                if upload.committed_offset() == upload.total_size() {
                    record_completion_failure(
                        CompletionFailureReason::RaceReloadFailed,
                        request_id,
                        upload_id,
                        Some(upload.project_id()),
                    );
                    Err(UploadServiceError::FinalizeFailed)
                } else {
                    Err(UploadServiceError::UploadIncomplete)
                }
            }
            Ok(Some(_)) => Err(UploadServiceError::UploadInactive),
            Ok(None) => Err(UploadServiceError::UploadNotFound),
            Err(_) => {
                record_completion_failure(
                    CompletionFailureReason::RaceReloadFailed,
                    request_id,
                    upload_id,
                    None,
                );
                Err(UploadServiceError::FinalizeFailed)
            }
        }
    }

    async fn complete_owned(
        &self,
        upload_id: Uuid,
        request_id: RequestId,
    ) -> Result<UploadRow, UploadServiceError> {
        let supervisor_service = self.clone();
        let supervisor_request_id = request_id.clone();
        let supervisor = tokio::spawn(async move {
            let operation_service = supervisor_service.clone();
            let operation_request_id = supervisor_request_id.clone();
            let operation = tokio::spawn(async move {
                operation_service
                    .complete(upload_id, &operation_request_id)
                    .await
            });
            match operation.await {
                Ok(result) => result,
                Err(_) => {
                    record_completion_failure(
                        CompletionFailureReason::TaskFailed,
                        &supervisor_request_id,
                        upload_id,
                        None,
                    );
                    Err(UploadServiceError::FinalizeFailed)
                }
            }
        });
        match supervisor.await {
            Ok(result) => result,
            Err(_) => {
                record_completion_failure(
                    CompletionFailureReason::SupervisorFailed,
                    &request_id,
                    upload_id,
                    None,
                );
                Err(UploadServiceError::FinalizeFailed)
            }
        }
    }

    /// Reconciles upload sessions left active or finalizing by an interrupted process.
    /// Call after database/storage initialization and before accepting requests.
    pub async fn recover_uploads(&self) -> Result<(), UploadRecoveryError> {
        let recoverable = match self.repository.recoverable_uploads().await {
            Ok(recoverable) => recoverable,
            Err(_) => {
                record_recovery_error("recoverable_query_failed", None, None);
                return Err(UploadRecoveryError);
            }
        };
        for upload in recoverable {
            if self.recover(upload.id()).await.is_err() {
                record_recovery_error(
                    "ambiguous_recovery_failure",
                    Some(upload.id()),
                    Some(upload.project_id()),
                );
                return Err(UploadRecoveryError);
            }
        }
        Ok(())
    }

    async fn recover(&self, upload_id: Uuid) -> Result<(), UploadRecoveryError> {
        let lock = self.lock_for(upload_id);
        let _guard = lock.lock().await;
        let Some(upload) = self
            .repository
            .get_upload(upload_id)
            .await
            .map_err(|_| UploadRecoveryError)?
        else {
            return Ok(());
        };
        if !matches!(
            upload.state(),
            UploadState::Active | UploadState::Finalizing
        ) {
            return Ok(());
        }
        let file_name = match SafeFileName::parse(upload.file_name()) {
            Ok(file_name) => file_name,
            Err(_) => {
                return self
                    .recovery_mark_failed(&upload, "invalid_stored_file_name")
                    .await;
            }
        };
        let destination_len = match self
            .storage
            .final_file_len(upload.project_id(), &file_name)
            .await
        {
            Ok(len) => len,
            Err(StorageError::UnsafeEntry) => {
                return self
                    .recovery_mark_failed(&upload, "unsafe_recovery_entry")
                    .await;
            }
            Err(_) => return Err(UploadRecoveryError),
        };
        let staging_len = match self.storage.staging_len(upload_id).await {
            Ok(len) => len,
            Err(StorageError::UnsafeEntry) => {
                return self
                    .recovery_mark_failed(&upload, "unsafe_recovery_entry")
                    .await;
            }
            Err(_) => return Err(UploadRecoveryError),
        };
        match upload.state() {
            UploadState::Active => {
                self.recover_active(&upload, destination_len, staging_len)
                    .await
            }
            UploadState::Finalizing => {
                self.recover_finalizing(&upload, &file_name, destination_len, staging_len)
                    .await
            }
            UploadState::Complete | UploadState::Failed => Ok(()),
        }
    }

    async fn recover_active(
        &self,
        upload: &UploadRow,
        destination_len: Option<u64>,
        staging_len: Option<u64>,
    ) -> Result<(), UploadRecoveryError> {
        if destination_len.is_some() {
            return self
                .recovery_mark_failed(upload, "active_destination_conflict")
                .await;
        }
        match staging_len {
            Some(len) if len == upload.committed_offset() => Ok(()),
            Some(len) if len > upload.committed_offset() => {
                self.storage
                    .truncate_staging(upload.id(), upload.committed_offset())
                    .await
                    .map_err(|_| UploadRecoveryError)?;
                self.storage
                    .sync_staging(upload.id())
                    .await
                    .map_err(|_| UploadRecoveryError)?;
                record_recovery_outcome(upload, "repaired", "active_staging_truncated");
                Ok(())
            }
            Some(_) => {
                self.recovery_mark_failed(upload, "active_staging_short")
                    .await
            }
            None => {
                self.recovery_mark_failed(upload, "active_staging_missing")
                    .await
            }
        }
    }

    async fn recover_finalizing(
        &self,
        upload: &UploadRow,
        file_name: &SafeFileName,
        destination_len: Option<u64>,
        staging_len: Option<u64>,
    ) -> Result<(), UploadRecoveryError> {
        match (destination_len, staging_len) {
            (Some(len), None) if len == upload.total_size() => {
                self.recovery_mark_complete(upload, "final_destination_completed")
                    .await
            }
            (Some(_), _) => {
                self.recovery_mark_failed(upload, "final_destination_conflict")
                    .await
            }
            (None, Some(len)) if len == upload.total_size() => {
                self.storage
                    .sync_staging(upload.id())
                    .await
                    .map_err(|_| UploadRecoveryError)?;
                match self
                    .storage
                    .finalize_no_replace(upload.id(), upload.project_id(), file_name)
                    .await
                {
                    Ok(()) => {}
                    Err(StorageError::AlreadyExists) => {
                        return self
                            .recovery_mark_failed(upload, "final_move_conflict")
                            .await;
                    }
                    Err(_) => return Err(UploadRecoveryError),
                }
                match self
                    .storage
                    .final_file_len(upload.project_id(), file_name)
                    .await
                    .map_err(|_| UploadRecoveryError)?
                {
                    Some(len) if len == upload.total_size() => {
                        self.recovery_mark_complete(upload, "final_staging_published")
                            .await
                    }
                    Some(_) => {
                        self.recovery_mark_failed(upload, "published_size_mismatch")
                            .await
                    }
                    None => Err(UploadRecoveryError),
                }
            }
            (None, Some(_)) => {
                self.recovery_mark_failed(upload, "final_staging_size_mismatch")
                    .await
            }
            (None, None) => {
                self.recovery_mark_failed(upload, "final_files_missing")
                    .await
            }
        }
    }

    async fn recovery_mark_failed(
        &self,
        upload: &UploadRow,
        reason: &'static str,
    ) -> Result<(), UploadRecoveryError> {
        self.repository
            .mark_upload_failed(upload.id(), reason)
            .await
            .map_err(|_| UploadRecoveryError)?;
        record_recovery_outcome(upload, "failed", reason);
        Ok(())
    }

    async fn recovery_mark_complete(
        &self,
        upload: &UploadRow,
        reason: &'static str,
    ) -> Result<(), UploadRecoveryError> {
        self.repository
            .mark_upload_complete(upload.id())
            .await
            .map_err(|_| UploadRecoveryError)?;
        record_recovery_outcome(upload, "repaired", reason);
        Ok(())
    }

    async fn write_chunk(
        &self,
        upload_id: Uuid,
        requested_offset: u64,
        declared_len: u64,
        reader: UploadBodyReader,
        request_id: &RequestId,
    ) -> Result<u64, ChunkError> {
        let lock = self.lock_for(upload_id);
        let _guard = lock.lock().await;
        let upload = self
            .repository
            .get_upload(upload_id)
            .await
            .map_err(|_| ChunkError::Unavailable { offset: None })?
            .ok_or(ChunkError::NotFound)?;
        let committed = upload.committed_offset();
        if declared_len > MAX_CHUNK_SIZE {
            return Err(ChunkError::PayloadTooLarge { offset: committed });
        }
        if upload.state() != UploadState::Active {
            return Err(ChunkError::Inactive { offset: committed });
        }
        if requested_offset < committed {
            return if retry_fully_committed(requested_offset, declared_len, committed) {
                Ok(committed)
            } else {
                Err(ChunkError::OffsetConflict { offset: committed })
            };
        }
        if requested_offset > committed {
            return Err(ChunkError::OffsetConflict { offset: committed });
        }
        let next = committed
            .checked_add(declared_len)
            .filter(|next| *next <= upload.total_size())
            .ok_or(ChunkError::TooLargeForUpload { offset: committed })?;
        let written = match self
            .storage
            .write_staging_chunk(upload_id, committed, reader)
            .await
        {
            Ok(written) => written,
            Err(error) => {
                return Err(self
                    .rollback_after_chunk_error(upload_id, committed, &error, request_id)
                    .await);
            }
        };
        if written != declared_len {
            return Err(self
                .rollback_after_length_error(upload_id, committed, request_id)
                .await);
        }
        let advance = self
            .repository
            .advance_upload_offset(upload_id, committed, next)
            .await;
        if matches!(advance, Ok(true)) {
            return Ok(next);
        }
        self.reconcile_conditional_advance(upload_id, committed, next, request_id)
            .await
    }

    async fn reconcile_conditional_advance(
        &self,
        upload_id: Uuid,
        previous: u64,
        next: u64,
        request_id: &RequestId,
    ) -> Result<u64, ChunkError> {
        let authoritative = match self.repository.get_upload(upload_id).await {
            Ok(Some(upload)) => upload,
            Ok(None) | Err(_) => {
                return Err(ChunkError::Unavailable { offset: None });
            }
        };
        let offset = authoritative.committed_offset();
        let staging_len = match self.storage.staging_len(upload_id).await {
            Ok(Some(len)) => len,
            _ => {
                self.mark_failed_after_recovery_error(
                    upload_id,
                    "staging inspection failed",
                    request_id,
                )
                .await;
                return Err(ChunkError::Unavailable {
                    offset: Some(offset),
                });
            }
        };
        if offset == next && staging_len == next {
            return Ok(next);
        }
        if staging_len < offset {
            self.mark_failed_after_recovery_error(upload_id, "staging offset mismatch", request_id)
                .await;
            return Err(ChunkError::Unavailable {
                offset: Some(offset),
            });
        }
        if self
            .storage
            .truncate_staging(upload_id, offset)
            .await
            .is_err()
        {
            self.mark_failed_after_recovery_error(upload_id, "chunk rollback failed", request_id)
                .await;
            return Err(ChunkError::Unavailable {
                offset: Some(offset),
            });
        }
        if offset == previous {
            Err(ChunkError::Unavailable {
                offset: Some(offset),
            })
        } else {
            Err(ChunkError::OffsetConflict { offset })
        }
    }

    async fn write_chunk_owned(
        &self,
        upload_id: Uuid,
        requested_offset: u64,
        declared_len: u64,
        reader: UploadBodyReader,
        request_id: RequestId,
    ) -> Result<u64, ChunkError> {
        let supervisor_service = self.clone();
        let supervisor_request_id = request_id.clone();
        let supervisor = tokio::spawn(async move {
            let operation_service = supervisor_service.clone();
            let operation_request_id = supervisor_request_id.clone();
            let operation = tokio::spawn(async move {
                operation_service
                    .write_chunk(
                        upload_id,
                        requested_offset,
                        declared_len,
                        reader,
                        &operation_request_id,
                    )
                    .await
            });
            match operation.await {
                Ok(result) => result,
                Err(_) => {
                    supervisor_service
                        .reconcile_chunk_task_failure(upload_id, &supervisor_request_id)
                        .await
                }
            }
        });
        match supervisor.await {
            Ok(result) => result,
            Err(_) => Err(ChunkError::Unavailable { offset: None }),
        }
    }

    async fn reconcile_chunk_task_failure(
        &self,
        upload_id: Uuid,
        request_id: &RequestId,
    ) -> Result<u64, ChunkError> {
        let lock = self.lock_for(upload_id);
        let _guard = lock.lock().await;
        let upload = self
            .repository
            .get_upload(upload_id)
            .await
            .map_err(|_| ChunkError::Unavailable { offset: None })?
            .ok_or(ChunkError::NotFound)?;
        let committed = upload.committed_offset();
        if self
            .storage
            .truncate_staging(upload_id, committed)
            .await
            .is_err()
        {
            self.mark_failed_after_recovery_error(upload_id, "chunk rollback failed", request_id)
                .await;
        }
        Err(ChunkError::Unavailable {
            offset: Some(committed),
        })
    }

    async fn rollback_after_chunk_error(
        &self,
        upload_id: Uuid,
        committed: u64,
        error: &StorageError,
        request_id: &RequestId,
    ) -> ChunkError {
        let kind = match error {
            StorageError::InvalidBody => ChunkRollbackKind::InvalidBody,
            StorageError::InsufficientSpace => ChunkRollbackKind::InsufficientStorage,
            _ => ChunkRollbackKind::Unavailable,
        };
        self.rollback_result(upload_id, committed, kind, request_id)
            .await
    }

    async fn rollback_after_length_error(
        &self,
        upload_id: Uuid,
        committed: u64,
        request_id: &RequestId,
    ) -> ChunkError {
        self.rollback_result(
            upload_id,
            committed,
            ChunkRollbackKind::InvalidBody,
            request_id,
        )
        .await
    }

    async fn rollback_result(
        &self,
        upload_id: Uuid,
        committed: u64,
        kind: ChunkRollbackKind,
        request_id: &RequestId,
    ) -> ChunkError {
        if self
            .storage
            .truncate_staging(upload_id, committed)
            .await
            .is_err()
        {
            self.mark_failed_after_recovery_error(upload_id, "chunk rollback failed", request_id)
                .await;
            return ChunkError::Unavailable {
                offset: Some(committed),
            };
        }
        match kind {
            ChunkRollbackKind::InvalidBody => ChunkError::InvalidLength { offset: committed },
            ChunkRollbackKind::InsufficientStorage => {
                ChunkError::InsufficientStorage { offset: committed }
            }
            ChunkRollbackKind::Unavailable => ChunkError::Unavailable {
                offset: Some(committed),
            },
        }
    }

    async fn mark_failed_after_recovery_error(
        &self,
        upload_id: Uuid,
        reason: &'static str,
        request_id: &RequestId,
    ) {
        if self
            .repository
            .mark_upload_failed(upload_id, reason)
            .await
            .is_err()
        {
            record_chunk_failure(ChunkFailureReason::FailureMarkFailed, request_id, upload_id);
        }
    }
}

fn retry_fully_committed(requested_offset: u64, declared_len: u64, committed: u64) -> bool {
    requested_offset
        .checked_add(declared_len)
        .is_some_and(|end| end <= committed)
}

#[derive(Clone, Copy)]
enum ChunkRollbackKind {
    InvalidBody,
    InsufficientStorage,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChunkFailureReason {
    FailureMarkFailed,
}

impl ChunkFailureReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::FailureMarkFailed => "failure_mark_failed",
        }
    }
}

fn record_chunk_failure(reason: ChunkFailureReason, request_id: &RequestId, upload_id: Uuid) {
    tracing::warn!(
        reason = reason.as_str(),
        request_id = %request_id,
        upload_id = %upload_id,
        "upload chunk recovery failed"
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChunkError {
    NotFound,
    PayloadTooLarge { offset: u64 },
    Inactive { offset: u64 },
    OffsetConflict { offset: u64 },
    TooLargeForUpload { offset: u64 },
    InvalidLength { offset: u64 },
    InsufficientStorage { offset: u64 },
    Unavailable { offset: Option<u64> },
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
    InvalidStaging,
    UploadIncomplete,
    UploadInactive,
    InsufficientStorage,
    CreateFailed,
    StatusFailed,
    FinalizeFailed,
    CleanupFailed,
}

/// A closed startup error indicating recovery could not safely determine or persist state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UploadRecoveryError;

impl std::fmt::Display for UploadRecoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("upload recovery could not be completed safely")
    }
}

impl std::error::Error for UploadRecoveryError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompletionFailureReason {
    DatabaseLookupFailed,
    InvalidStoredFileName,
    MarkFinalizingFailed,
    DestinationConflict,
    StagingInvalid,
    InsufficientSpace,
    StorageUnavailable,
    FinalSizeMismatch,
    FailureMarkFailed,
    CompleteTransitionFailed,
    CompleteReloadFailed,
    RaceReloadFailed,
    TaskFailed,
    SupervisorFailed,
}

impl CompletionFailureReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::DatabaseLookupFailed => "database_upload_lookup_failed",
            Self::InvalidStoredFileName => "invalid_stored_file_name",
            Self::MarkFinalizingFailed => "mark_finalizing_failed",
            Self::DestinationConflict => "destination_conflict",
            Self::StagingInvalid => "staging_invalid",
            Self::InsufficientSpace => "insufficient_space",
            Self::StorageUnavailable => "storage_unavailable",
            Self::FinalSizeMismatch => "final_size_mismatch",
            Self::FailureMarkFailed => "failure_mark_failed",
            Self::CompleteTransitionFailed => "complete_transition_failed",
            Self::CompleteReloadFailed => "complete_reload_failed",
            Self::RaceReloadFailed => "race_reload_failed",
            Self::TaskFailed => "finalization_task_failed",
            Self::SupervisorFailed => "finalization_supervisor_failed",
        }
    }
}

fn completion_storage_failure(error: &StorageError) -> CompletionFailureReason {
    match error {
        StorageError::InsufficientSpace => CompletionFailureReason::InsufficientSpace,
        _ => CompletionFailureReason::StorageUnavailable,
    }
}

fn finalization_storage_error(error: &StorageError) -> UploadServiceError {
    match error {
        StorageError::InsufficientSpace => UploadServiceError::InsufficientStorage,
        _ => UploadServiceError::FinalizeFailed,
    }
}

fn record_completion_failure(
    reason: CompletionFailureReason,
    request_id: &RequestId,
    upload_id: Uuid,
    project_id: Option<Uuid>,
) {
    tracing::warn!(
        reason = reason.as_str(),
        request_id = %request_id,
        upload_id = %upload_id,
        project_id = project_id.map(|id| id.to_string()).as_deref().unwrap_or("unknown"),
        "upload finalization failed"
    );
}

fn record_recovery_outcome(upload: &UploadRow, outcome: &'static str, reason: &'static str) {
    tracing::info!(
        outcome,
        reason,
        upload_id = %upload.id(),
        project_id = %upload.project_id(),
        "upload startup recovery outcome"
    );
}

fn record_recovery_error(reason: &'static str, upload_id: Option<Uuid>, project_id: Option<Uuid>) {
    tracing::error!(
        outcome = "error",
        reason,
        upload_id = upload_id
            .map(|id| id.to_string())
            .as_deref()
            .unwrap_or("unknown"),
        project_id = project_id
            .map(|id| id.to_string())
            .as_deref()
            .unwrap_or("unknown"),
        "upload startup recovery stopped"
    );
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
        | StorageError::InvalidBody
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
        .route("/api/v1/uploads/{upload_id}/chunk", put(upload_chunk))
        .route(
            "/api/v1/uploads/{upload_id}/complete",
            post(complete_upload),
        )
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

async fn complete_upload(
    State(service): State<UploadService>,
    Extension(request_id): Extension<RequestId>,
    Extension(_owner): Extension<OwnerIdentity>,
    Path(upload_id): Path<String>,
) -> Result<Json<UploadResponse>, AppError> {
    let upload_id =
        parse_canonical_uuid(&upload_id).ok_or_else(|| invalid_request(request_id.clone()))?;
    service
        .complete_owned(upload_id, request_id.clone())
        .await
        .map(|upload| Json(upload.into()))
        .map_err(|error| map_service_error(error, request_id))
}

const MAX_CHUNK_SIZE: u64 = 32 * 1024 * 1024;

async fn upload_chunk(
    State(service): State<UploadService>,
    Extension(request_id): Extension<RequestId>,
    Extension(_owner): Extension<OwnerIdentity>,
    Path(upload_id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(upload_id) = parse_canonical_uuid(&upload_id) else {
        return invalid_request(request_id).into_response();
    };
    let Some(requested_offset) = parse_single_decimal_header(&headers, "upload-offset") else {
        return invalid_request(request_id).into_response();
    };
    let Some(declared_len) = parse_single_decimal_header(&headers, header::CONTENT_LENGTH) else {
        return invalid_request(request_id).into_response();
    };
    if headers.get_all(header::CONTENT_TYPE).iter().count() != 1
        || headers.get(header::CONTENT_TYPE)
            != Some(&HeaderValue::from_static("application/octet-stream"))
    {
        return invalid_request(request_id).into_response();
    }
    let stream = body
        .into_data_stream()
        .map_err(|error| io::Error::other(error.to_string()));
    let reader: UploadBodyReader = Box::pin(StreamReader::new(stream).take(declared_len + 1));
    match service
        .write_chunk_owned(
            upload_id,
            requested_offset,
            declared_len,
            reader,
            request_id.clone(),
        )
        .await
    {
        Ok(offset) => response_with_upload_offset(StatusCode::NO_CONTENT.into_response(), offset),
        Err(error) => map_chunk_error(error, request_id),
    }
}

fn parse_single_decimal_header(
    headers: &HeaderMap,
    name: impl axum::http::header::AsHeaderName,
) -> Option<u64> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let value = value.to_str().ok()?;
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    let value = value.parse::<u64>().ok()?;
    (value <= i64::MAX as u64).then_some(value)
}

fn map_chunk_error(error: ChunkError, request_id: RequestId) -> Response {
    let offset = match error {
        ChunkError::NotFound => {
            return AppError::not_found(
                request_id,
                "upload_not_found",
                "The upload was not found.",
            )
            .into_response();
        }
        ChunkError::PayloadTooLarge { offset } => offset,
        ChunkError::Inactive { offset } => offset,
        ChunkError::OffsetConflict { offset } => offset,
        ChunkError::TooLargeForUpload { offset } => offset,
        ChunkError::InvalidLength { offset } => offset,
        ChunkError::InsufficientStorage { offset } => offset,
        ChunkError::Unavailable { offset: None } => {
            return AppError::service_unavailable(
                request_id,
                "upload_chunk_failed",
                "The upload chunk could not be stored safely.",
            )
            .into_response();
        }
        ChunkError::Unavailable {
            offset: Some(offset),
        } => offset,
    };
    let response = match error {
        ChunkError::PayloadTooLarge { .. } => AppError::payload_too_large(request_id),
        ChunkError::Inactive { .. } => AppError::conflict(
            request_id,
            "upload_not_active",
            "The upload is not active.",
            None,
        ),
        ChunkError::OffsetConflict { .. } => AppError::conflict(
            request_id,
            "upload_offset_conflict",
            "The upload offset does not match the committed offset.",
            Some(crate::error::SafeErrorDetails::expected_offset(offset)),
        ),
        ChunkError::TooLargeForUpload { .. } => AppError::conflict(
            request_id,
            "upload_size_exceeded",
            "The chunk would exceed the upload size.",
            None,
        ),
        ChunkError::InvalidLength { .. } => invalid_request(request_id),
        ChunkError::InsufficientStorage { .. } => AppError::insufficient_storage(request_id),
        ChunkError::Unavailable { .. } => AppError::service_unavailable(
            request_id,
            "upload_chunk_failed",
            "The upload chunk could not be stored safely.",
        ),
        ChunkError::NotFound => unreachable!("handled above"),
    }
    .into_response();
    response_with_upload_offset(response, offset)
}

fn response_with_upload_offset(mut response: Response, offset: u64) -> Response {
    response.headers_mut().insert(
        "upload-offset",
        HeaderValue::from_str(&offset.to_string())
            .expect("a nonnegative integer is a valid header value"),
    );
    response
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
        UploadServiceError::InvalidStaging => AppError::conflict(
            request_id,
            "upload_staging_invalid",
            "The upload staging data is inconsistent.",
            None,
        ),
        UploadServiceError::UploadIncomplete => AppError::conflict(
            request_id,
            "upload_incomplete",
            "The upload has not received all expected bytes.",
            None,
        ),
        UploadServiceError::UploadInactive => AppError::conflict(
            request_id,
            "upload_not_active",
            "The upload cannot be completed from its current state.",
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
        UploadServiceError::FinalizeFailed => AppError::service_unavailable(
            request_id,
            "upload_finalize_failed",
            "The upload could not be finalized safely.",
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
    use super::{ChunkFailureReason, UploadFailureReason, retry_fully_committed};

    #[test]
    fn committed_retry_span_checks_boundary_and_overflow() {
        assert!(retry_fully_committed(1, 3, 4));
        assert!(!retry_fully_committed(1, 4, 4));
        assert!(!retry_fully_committed(u64::MAX - 1, 2, u64::MAX));
    }

    #[test]
    fn failure_reasons_are_closed_safe_literals() {
        assert_eq!(
            ChunkFailureReason::FailureMarkFailed.as_str(),
            "failure_mark_failed"
        );
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

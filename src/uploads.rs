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

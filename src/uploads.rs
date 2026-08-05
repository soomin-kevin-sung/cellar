//! Project file uploads, including sequential chunks for large files.

use std::{
    collections::HashMap,
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{
        Path, Query, State,
        rejection::{PathRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode, header},
    routing::{delete, post, put},
};
use futures_util::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};
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

/// Exact staging operations needed to publish upload request bodies.
pub trait UploadStorage: Send + Sync {
    fn cleanup_staging(&self) -> UploadStorageFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

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

    fn write_chunk_at<'a>(
        &'a self,
        _upload_id: Uuid,
        _offset: u64,
        _reader: UploadBodyReader,
    ) -> UploadStorageFuture<'a, u64> {
        Box::pin(async { Err(StorageError::NotFound) })
    }

    fn destination_exists<'a>(
        &'a self,
        _project_id: Uuid,
        _name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, bool> {
        Box::pin(async { Err(StorageError::NotFound) })
    }
}

impl UploadStorage for Storage {
    fn cleanup_staging(&self) -> UploadStorageFuture<'_, ()> {
        Box::pin(async move { self.cleanup_staging().await })
    }

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

    fn write_chunk_at<'a>(
        &'a self,
        upload_id: Uuid,
        offset: u64,
        reader: UploadBodyReader,
    ) -> UploadStorageFuture<'a, u64> {
        Box::pin(async move { self.write_chunk(upload_id, offset, reader).await })
    }

    fn destination_exists<'a>(
        &'a self,
        project_id: Uuid,
        name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, bool> {
        Box::pin(async move { self.destination_exists(project_id, name).await })
    }
}

const CHUNK_SIZE: u64 = 32 * 1024 * 1024;

#[derive(Debug)]
struct ChunkUpload {
    project_id: Uuid,
    name: SafeFileName,
    total_size: u64,
    committed_offset: u64,
}

#[derive(Clone)]
pub struct UploadService {
    repository: Arc<dyn UploadRepository>,
    storage: Arc<dyn UploadStorage>,
    chunk_uploads: Arc<tokio::sync::Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<ChunkUpload>>>>>,
}

impl UploadService {
    pub fn new(repository: Arc<dyn UploadRepository>, storage: Arc<dyn UploadStorage>) -> Self {
        Self {
            repository,
            storage,
            chunk_uploads: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Removes temporary upload files left behind by a previous process exit.
    pub async fn recover_uploads(&self) -> Result<(), UploadRecoveryError> {
        self.storage
            .cleanup_staging()
            .await
            .map_err(|_| UploadRecoveryError)
    }

    async fn upload(
        &self,
        project_id: Uuid,
        file_name: String,
        body: Body,
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
        let upload_id = Uuid::now_v7();
        let cancellation = UploadCancellation::new();
        let reader = cancellation_aware_reader(body, cancellation.subscribe());
        let operation_gate = Arc::new(tokio::sync::Mutex::new(()));
        let staging_owned = Arc::new(AtomicBool::new(false));
        let mut waiter = UploadWaiterCleanup::new(
            storage.clone(),
            upload_id,
            cancellation.clone(),
            operation_gate.clone(),
            staging_owned.clone(),
        );
        let request_id = request_id.clone();
        let worker_gate = operation_gate.clone();
        let worker_cancellation = cancellation.clone();
        let operation = tokio::spawn(async move {
            let _operation_guard = worker_gate.lock().await;
            OwnedUploadOperation {
                storage,
                upload_id,
                project_id,
                name,
                reader,
                request_id,
                cancellation: worker_cancellation,
                operation_gate,
                staging_owned,
            }
            .run()
            .await
        });
        match operation.await {
            Ok(result) => {
                waiter.disarm();
                result
            }
            Err(_) => Err(UploadServiceError::Unavailable),
        }
    }

    async fn start_chunk_upload(
        &self,
        project_id: Uuid,
        file_name: String,
        total_size: u64,
        request_id: &RequestId,
    ) -> Result<StartChunkUploadResponse, UploadServiceError> {
        if total_size == 0 {
            return Err(UploadServiceError::InvalidRequest);
        }
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
        match self.storage.destination_exists(project_id, &name).await {
            Ok(true) => return Err(UploadServiceError::Conflict),
            Ok(false) => {}
            Err(error) => return Err(map_internal_storage_error(&error)),
        }

        let upload_id = Uuid::now_v7();
        self.storage
            .create_staging(upload_id)
            .await
            .map_err(|error| map_internal_storage_error(&error))?;
        self.chunk_uploads.lock().await.insert(
            upload_id,
            Arc::new(tokio::sync::Mutex::new(ChunkUpload {
                project_id,
                name,
                total_size,
                committed_offset: 0,
            })),
        );
        Ok(StartChunkUploadResponse {
            upload_id,
            chunk_size: DecimalU64(CHUNK_SIZE),
            offset: DecimalU64(0),
        })
    }

    async fn write_chunk(
        &self,
        upload_id: Uuid,
        offset: u64,
        body: Body,
        request_id: &RequestId,
    ) -> Result<ChunkUploadResponse, UploadServiceError> {
        let session = self
            .chunk_uploads
            .lock()
            .await
            .get(&upload_id)
            .cloned()
            .ok_or(UploadServiceError::UploadNotFound)?;
        let mut upload = session.lock().await;
        if offset != upload.committed_offset || offset >= upload.total_size {
            return Err(self
                .fail_chunk_upload(
                    upload_id,
                    upload.project_id,
                    UploadServiceError::OffsetConflict,
                    request_id,
                )
                .await);
        }
        let allowed = CHUNK_SIZE.min(upload.total_size - offset);
        let reader = Box::pin(body_reader(body).take(allowed + 1));
        let written = match self.storage.write_chunk_at(upload_id, offset, reader).await {
            Ok(written) if written > 0 && written <= allowed => written,
            Ok(_) => {
                return Err(self
                    .fail_chunk_upload(
                        upload_id,
                        upload.project_id,
                        UploadServiceError::InvalidRequest,
                        request_id,
                    )
                    .await);
            }
            Err(error) => {
                let mapped = map_write_error(&error);
                record_storage_failure(&error, request_id, upload.project_id);
                return Err(self
                    .fail_chunk_upload(upload_id, upload.project_id, mapped, request_id)
                    .await);
            }
        };
        upload.committed_offset = offset + written;
        Ok(ChunkUploadResponse {
            offset: DecimalU64(upload.committed_offset),
        })
    }

    async fn complete_chunk_upload(
        &self,
        upload_id: Uuid,
        request_id: &RequestId,
    ) -> Result<UploadResponse, UploadServiceError> {
        let session = self
            .chunk_uploads
            .lock()
            .await
            .get(&upload_id)
            .cloned()
            .ok_or(UploadServiceError::UploadNotFound)?;
        let upload = session.lock().await;
        if upload.committed_offset != upload.total_size {
            return Err(self
                .fail_chunk_upload(
                    upload_id,
                    upload.project_id,
                    UploadServiceError::OffsetConflict,
                    request_id,
                )
                .await);
        }
        if let Err(error) = self.storage.sync_staging(upload_id).await {
            let mapped = map_internal_storage_error(&error);
            record_storage_failure(&error, request_id, upload.project_id);
            return Err(self
                .fail_chunk_upload(upload_id, upload.project_id, mapped, request_id)
                .await);
        }
        if let Err(error) = self
            .storage
            .finalize_no_replace(upload_id, upload.project_id, &upload.name)
            .await
        {
            let mapped = map_finalization_error(&error);
            record_storage_failure(&error, request_id, upload.project_id);
            self.chunk_uploads.lock().await.remove(&upload_id);
            if self.storage.remove_staging(upload_id).await.is_err() {
                return Err(UploadServiceError::Unavailable);
            }
            return Err(mapped);
        }
        let response = UploadResponse {
            name: upload.name.as_str().to_owned(),
            size: DecimalU64(upload.total_size),
        };
        self.chunk_uploads.lock().await.remove(&upload_id);
        Ok(response)
    }

    async fn abort_chunk_upload(&self, upload_id: Uuid) -> Result<(), UploadServiceError> {
        if self.chunk_uploads.lock().await.remove(&upload_id).is_none() {
            return Ok(());
        }
        self.storage
            .remove_staging(upload_id)
            .await
            .map_err(|error| map_internal_storage_error(&error))
    }

    async fn fail_chunk_upload(
        &self,
        upload_id: Uuid,
        project_id: Uuid,
        original: UploadServiceError,
        request_id: &RequestId,
    ) -> UploadServiceError {
        self.chunk_uploads.lock().await.remove(&upload_id);
        match self.storage.remove_staging(upload_id).await {
            Ok(()) => original,
            Err(_) => {
                record_failure("staging_cleanup_failed", request_id, project_id);
                UploadServiceError::Unavailable
            }
        }
    }
}

struct OwnedUploadOperation {
    storage: Arc<dyn UploadStorage>,
    upload_id: Uuid,
    project_id: Uuid,
    name: SafeFileName,
    reader: UploadBodyReader,
    request_id: RequestId,
    cancellation: UploadCancellation,
    operation_gate: Arc<tokio::sync::Mutex<()>>,
    staging_owned: Arc<AtomicBool>,
}

impl OwnedUploadOperation {
    async fn run(self) -> Result<UploadResponse, UploadServiceError> {
        let Self {
            storage,
            upload_id,
            project_id,
            name,
            reader,
            request_id,
            cancellation,
            operation_gate,
            staging_owned,
        } = self;
        if cancellation.is_cancelled() {
            return Err(UploadServiceError::InvalidRequest);
        }
        if let Err(error) = storage.create_staging(upload_id).await {
            return Err(map_internal_storage_error(&error));
        }
        staging_owned.store(true, Ordering::Release);
        let mut cleanup = StagingCleanup::new(
            storage.clone(),
            upload_id,
            operation_gate,
            staging_owned.clone(),
        );
        if cancellation.is_cancelled() {
            return Err(cleanup_result(
                &mut cleanup,
                UploadServiceError::InvalidRequest,
                &request_id,
                project_id,
            )
            .await);
        }

        let size = match storage.write_upload(upload_id, reader).await {
            Ok(size) => size,
            Err(error) => {
                let mapped = map_write_error(&error);
                record_storage_failure(&error, &request_id, project_id);
                return Err(cleanup_result(&mut cleanup, mapped, &request_id, project_id).await);
            }
        };
        if cancellation.is_cancelled() {
            return Err(cleanup_result(
                &mut cleanup,
                UploadServiceError::InvalidRequest,
                &request_id,
                project_id,
            )
            .await);
        }
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
        if cancellation.is_cancelled() {
            return Err(cleanup_result(
                &mut cleanup,
                UploadServiceError::InvalidRequest,
                &request_id,
                project_id,
            )
            .await);
        }

        let finalization = storage
            .finalize_no_replace(upload_id, project_id, &name)
            .await;
        if let Err(error) = finalization {
            let mapped = map_finalization_error(&error);
            record_storage_failure(&error, &request_id, project_id);
            return Err(cleanup_result(&mut cleanup, mapped, &request_id, project_id).await);
        }

        staging_owned.store(false, Ordering::Release);
        cleanup.disarm();
        Ok(UploadResponse {
            name: name.as_str().to_owned(),
            size: DecimalU64(size),
        })
    }
}

#[derive(Clone)]
struct UploadCancellation {
    sender: tokio::sync::watch::Sender<bool>,
}

impl UploadCancellation {
    fn new() -> Self {
        let (sender, _) = tokio::sync::watch::channel(false);
        Self { sender }
    }

    fn cancel(&self) {
        self.sender.send_replace(true);
    }

    fn is_cancelled(&self) -> bool {
        *self.sender.borrow()
    }

    fn subscribe(&self) -> tokio::sync::watch::Receiver<bool> {
        self.sender.subscribe()
    }
}

fn cancellation_aware_reader(
    body: Body,
    cancellation: tokio::sync::watch::Receiver<bool>,
) -> UploadBodyReader {
    let stream = body
        .into_data_stream()
        .map_err(|error| io::Error::other(error.to_string()));
    let stream = futures_util::stream::unfold(
        (Box::pin(stream), cancellation, false),
        |(mut stream, mut cancellation, finished)| async move {
            if finished {
                return None;
            }
            if *cancellation.borrow() {
                return Some((Err(cancelled_body_error()), (stream, cancellation, true)));
            }
            tokio::select! {
                item = stream.next() => item.map(|item| (item, (stream, cancellation, false))),
                changed = cancellation.changed() => {
                    if changed.is_ok() && *cancellation.borrow() {
                        Some((Err(cancelled_body_error()), (stream, cancellation, true)))
                    } else {
                        None
                    }
                }
            }
        },
    );
    Box::pin(StreamReader::new(stream))
}

fn body_reader(body: Body) -> UploadBodyReader {
    let stream = body
        .into_data_stream()
        .map_err(|error| io::Error::other(error.to_string()));
    Box::pin(StreamReader::new(stream))
}

fn cancelled_body_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "upload request was cancelled")
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
    operation_gate: Arc<tokio::sync::Mutex<()>>,
    staging_owned: Arc<AtomicBool>,
    armed: bool,
}

impl StagingCleanup {
    fn new(
        storage: Arc<dyn UploadStorage>,
        upload_id: Uuid,
        operation_gate: Arc<tokio::sync::Mutex<()>>,
        staging_owned: Arc<AtomicBool>,
    ) -> Self {
        Self {
            storage,
            upload_id,
            operation_gate,
            staging_owned,
            armed: true,
        }
    }

    async fn cleanup(&mut self) -> Result<(), StorageError> {
        if !self.staging_owned.load(Ordering::Acquire) {
            self.disarm();
            return Ok(());
        }
        match self.storage.remove_staging(self.upload_id).await {
            Ok(()) => {
                self.staging_owned.store(false, Ordering::Release);
                self.disarm();
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let storage = self.storage.clone();
        let upload_id = self.upload_id;
        let operation_gate = self.operation_gate.clone();
        let staging_owned = self.staging_owned.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _guard = operation_gate.lock().await;
                if !staging_owned.load(Ordering::Acquire) {
                    return;
                }
                if storage.remove_staging(upload_id).await.is_ok() {
                    staging_owned.store(false, Ordering::Release);
                } else {
                    tracing::warn!(upload_id = %upload_id, "staging cleanup retry failed");
                }
            });
        }
    }
}

struct UploadWaiterCleanup {
    storage: Arc<dyn UploadStorage>,
    upload_id: Uuid,
    cancellation: UploadCancellation,
    operation_gate: Arc<tokio::sync::Mutex<()>>,
    staging_owned: Arc<AtomicBool>,
    armed: bool,
}

impl UploadWaiterCleanup {
    fn new(
        storage: Arc<dyn UploadStorage>,
        upload_id: Uuid,
        cancellation: UploadCancellation,
        operation_gate: Arc<tokio::sync::Mutex<()>>,
        staging_owned: Arc<AtomicBool>,
    ) -> Self {
        Self {
            storage,
            upload_id,
            cancellation,
            operation_gate,
            staging_owned,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for UploadWaiterCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.cancellation.cancel();
        let storage = self.storage.clone();
        let upload_id = self.upload_id;
        let operation_gate = self.operation_gate.clone();
        let staging_owned = self.staging_owned.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _guard = operation_gate.lock().await;
                if !staging_owned.load(Ordering::Acquire) {
                    return;
                }
                if storage.remove_staging(upload_id).await.is_ok() {
                    staging_owned.store(false, Ordering::Release);
                } else {
                    tracing::warn!(upload_id = %upload_id, "waiter cleanup retry failed");
                }
            });
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadServiceError {
    InvalidRequest,
    ProjectNotFound,
    UploadNotFound,
    OffsetConflict,
    Conflict,
    InsufficientStorage,
    Unavailable,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UploadQuery {
    file_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StartChunkUploadRequest {
    file_name: String,
    total_size: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChunkOffsetQuery {
    offset: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StartChunkUploadResponse {
    upload_id: Uuid,
    chunk_size: DecimalU64,
    offset: DecimalU64,
}

#[derive(Serialize)]
struct ChunkUploadResponse {
    offset: DecimalU64,
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
        .route(
            "/api/v1/projects/{project_id}/upload-sessions",
            post(start_chunk_upload),
        )
        .route(
            "/api/v1/upload-sessions/{upload_id}/chunks",
            put(write_chunk),
        )
        .route(
            "/api/v1/upload-sessions/{upload_id}/complete",
            post(complete_chunk_upload),
        )
        .route(
            "/api/v1/upload-sessions/{upload_id}",
            delete(abort_chunk_upload),
        )
        .with_state(service)
}

async fn start_chunk_upload(
    State(service): State<UploadService>,
    Extension(request_id): Extension<RequestId>,
    Extension(_owner): Extension<OwnerIdentity>,
    path: Result<Path<String>, PathRejection>,
    payload: Result<Json<StartChunkUploadRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<StartChunkUploadResponse>), AppError> {
    let Path(project_id) = path.map_err(|_| invalid_request(request_id.clone()))?;
    let project_id =
        parse_canonical_uuid(&project_id).ok_or_else(|| invalid_request(request_id.clone()))?;
    let Json(payload) = payload.map_err(|_| invalid_request(request_id.clone()))?;
    let total_size = payload
        .total_size
        .parse::<u64>()
        .map_err(|_| invalid_request(request_id.clone()))?;
    let response = service
        .start_chunk_upload(project_id, payload.file_name, total_size, &request_id)
        .await
        .map_err(|error| upload_error(error, request_id))?;
    Ok((StatusCode::CREATED, Json(response)))
}

async fn write_chunk(
    State(service): State<UploadService>,
    Extension(request_id): Extension<RequestId>,
    Extension(_owner): Extension<OwnerIdentity>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<ChunkOffsetQuery>, QueryRejection>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<ChunkUploadResponse>, AppError> {
    let Path(upload_id) = path.map_err(|_| invalid_request(request_id.clone()))?;
    let upload_id =
        parse_canonical_uuid(&upload_id).ok_or_else(|| invalid_request(request_id.clone()))?;
    let Query(query) = query.map_err(|_| invalid_request(request_id.clone()))?;
    let offset = query
        .offset
        .parse::<u64>()
        .map_err(|_| invalid_request(request_id.clone()))?;
    require_octet_stream(&headers).map_err(|()| invalid_request(request_id.clone()))?;
    service
        .write_chunk(upload_id, offset, body, &request_id)
        .await
        .map(Json)
        .map_err(|error| upload_error(error, request_id))
}

async fn complete_chunk_upload(
    State(service): State<UploadService>,
    Extension(request_id): Extension<RequestId>,
    Extension(_owner): Extension<OwnerIdentity>,
    path: Result<Path<String>, PathRejection>,
) -> Result<(StatusCode, Json<UploadResponse>), AppError> {
    let Path(upload_id) = path.map_err(|_| invalid_request(request_id.clone()))?;
    let upload_id =
        parse_canonical_uuid(&upload_id).ok_or_else(|| invalid_request(request_id.clone()))?;
    let response = service
        .complete_chunk_upload(upload_id, &request_id)
        .await
        .map_err(|error| upload_error(error, request_id))?;
    Ok((StatusCode::CREATED, Json(response)))
}

async fn abort_chunk_upload(
    State(service): State<UploadService>,
    Extension(request_id): Extension<RequestId>,
    Extension(_owner): Extension<OwnerIdentity>,
    path: Result<Path<String>, PathRejection>,
) -> Result<StatusCode, AppError> {
    let Path(upload_id) = path.map_err(|_| invalid_request(request_id.clone()))?;
    let upload_id =
        parse_canonical_uuid(&upload_id).ok_or_else(|| invalid_request(request_id.clone()))?;
    service
        .abort_chunk_upload(upload_id)
        .await
        .map_err(|error| upload_error(error, request_id))?;
    Ok(StatusCode::NO_CONTENT)
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

    let response = service
        .upload(project_id, query.file_name, body, &request_id)
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
        UploadServiceError::UploadNotFound => {
            AppError::not_found(request_id, "upload_not_found", "The upload was not found.")
        }
        UploadServiceError::OffsetConflict => AppError::conflict(
            request_id,
            "upload_offset_conflict",
            "The upload offset does not match the committed offset.",
            None,
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

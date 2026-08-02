use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use sha2::{Digest as _, Sha256};
use time::{Duration, OffsetDateTime};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::{FileEntryId, ProjectId, UploadId};

pub const DEFAULT_MAX_CHUNK_SIZE: i64 = 32 * 1024 * 1024;
pub const DEFAULT_MAX_ACTIVE_SESSIONS: u32 = 8;
pub const DEFAULT_MAX_CONCURRENT_UPLOADS: u32 = 3;
pub const DEFAULT_FREE_SPACE_RESERVE: i64 = 5 * 1024 * 1024 * 1024;
pub const DEFAULT_UPLOAD_TTL_DAYS: i64 = 7;
pub const MAX_UPLOAD_NAME_BYTES: usize = 1_024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UploadState {
    Created,
    Uploading,
    Verifying,
    Committing,
    Complete,
    Failed,
    Cancelled,
}

impl UploadState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Uploading => "uploading",
            Self::Verifying => "verifying",
            Self::Committing => "committing",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Created | Self::Uploading | Self::Verifying | Self::Committing
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingChunk {
    pub offset: i64,
    pub length: i64,
    pub digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadSession {
    pub id: UploadId,
    pub project_id: ProjectId,
    pub destination_parent_id: Option<FileEntryId>,
    pub destination_name: String,
    pub expected_size: i64,
    pub committed_offset: i64,
    pub expected_hash: Option<[u8; 32]>,
    pub pending: Option<PendingChunk>,
    pub state: UploadState,
    pub expires_at: OffsetDateTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewUpload {
    pub project_id: ProjectId,
    pub destination_parent_id: Option<FileEntryId>,
    pub destination_name: String,
    pub expected_size: i64,
    pub expected_hash: Option<[u8; 32]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UploadLimits {
    pub max_chunk_size: i64,
    pub max_active_sessions: u32,
    pub max_concurrent_uploads: u32,
    pub free_space_reserve: i64,
    pub session_ttl: Duration,
}

impl Default for UploadLimits {
    fn default() -> Self {
        Self {
            max_chunk_size: DEFAULT_MAX_CHUNK_SIZE,
            max_active_sessions: DEFAULT_MAX_ACTIVE_SESSIONS,
            max_concurrent_uploads: DEFAULT_MAX_CONCURRENT_UPLOADS,
            free_space_reserve: DEFAULT_FREE_SPACE_RESERVE,
            session_ttl: Duration::days(DEFAULT_UPLOAD_TTL_DAYS),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UploadRepositoryError {
    NotFound,
    Conflict,
    Expired,
    TooManySessions,
    TooManyConcurrent,
    InsufficientStorage,
    Unavailable,
}

#[async_trait]
pub trait UploadRepository: Send + Sync {
    async fn create(
        &self,
        session: &UploadSession,
        max_active_sessions: u32,
        reservation_capacity: i64,
    ) -> Result<(), UploadRepositoryError>;
    async fn read(&self, id: UploadId) -> Result<UploadSession, UploadRepositoryError>;
    async fn prepare_chunk(
        &self,
        id: UploadId,
        offset: i64,
        length: i64,
        digest: [u8; 32],
        max_concurrent_uploads: u32,
    ) -> Result<UploadSession, UploadRepositoryError>;
    async fn commit_chunk(
        &self,
        id: UploadId,
        offset: i64,
        length: i64,
        digest: [u8; 32],
    ) -> Result<UploadSession, UploadRepositoryError>;
    async fn clear_pending(&self, id: UploadId) -> Result<UploadSession, UploadRepositoryError>;
    async fn fail(&self, id: UploadId) -> Result<(), UploadRepositoryError>;
    async fn cancel(&self, id: UploadId) -> Result<(), UploadRepositoryError>;
    async fn active_ids(&self) -> Result<Vec<UploadId>, UploadRepositoryError>;
    async fn expire_if_due(
        &self,
        id: UploadId,
        now: OffsetDateTime,
    ) -> Result<bool, UploadRepositoryError>;
    async fn fail_pristine_created(&self, id: UploadId) -> Result<bool, UploadRepositoryError>;
    async fn cleanup_ids(&self) -> Result<Vec<UploadId>, UploadRepositoryError>;
    async fn complete_cleanup(&self, id: UploadId) -> Result<(), UploadRepositoryError>;
    async fn staging_identity(
        &self,
        id: UploadId,
    ) -> Result<Option<StagingIdentity>, UploadRepositoryError>;
    async fn record_staging_identity(
        &self,
        id: UploadId,
        identity: StagingIdentity,
    ) -> Result<(), UploadRepositoryError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UploadStagingError {
    NotFound,
    InsufficientStorage,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StagingIdentity([u8; 24]);

impl StagingIdentity {
    #[must_use]
    pub const fn new(bytes: [u8; 24]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(self) -> [u8; 24] {
        self.0
    }
}

#[async_trait]
pub trait UploadStagingStore: Send + Sync {
    async fn create(&self, id: UploadId) -> Result<(), UploadStagingError>;
    async fn length(&self, id: UploadId) -> Result<i64, UploadStagingError>;
    async fn read_exact(
        &self,
        id: UploadId,
        offset: i64,
        length: i64,
    ) -> Result<Vec<u8>, UploadStagingError>;
    async fn truncate(&self, id: UploadId, length: i64) -> Result<(), UploadStagingError>;
    /// Writes the entire range at `offset` and completes a durable file flush
    /// before returning success (FlushFileBuffers on Windows).
    async fn write_exact_and_flush(
        &self,
        id: UploadId,
        offset: i64,
        bytes: &[u8],
    ) -> Result<(), UploadStagingError>;
    async fn write_verify_and_flush(
        &self,
        id: UploadId,
        offset: i64,
        bytes: &[u8],
        digest: [u8; 32],
    ) -> Result<i64, UploadStagingError> {
        self.write_exact_and_flush(id, offset, bytes).await?;
        let length = self.length(id).await?;
        let durable = self
            .read_exact(
                id,
                offset,
                i64::try_from(bytes.len()).map_err(|_| UploadStagingError::Unavailable)?,
            )
            .await?;
        if sha256(&durable) == digest {
            Ok(length)
        } else {
            Err(UploadStagingError::Unavailable)
        }
    }
    async fn remove(&self, id: UploadId) -> Result<(), UploadStagingError>;
    async fn available_space(&self) -> Result<i64, UploadStagingError>;
    async fn identity(&self, _id: UploadId) -> Result<Option<StagingIdentity>, UploadStagingError> {
        Ok(None)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UploadServiceError {
    Invalid,
    NotFound,
    Conflict,
    Expired,
    TooManyRequests,
    InsufficientStorage,
    Unavailable,
}

impl UploadServiceError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Invalid => "invalid_upload_request",
            Self::NotFound => "upload_not_found",
            Self::Conflict => "upload_conflict",
            Self::Expired => "upload_expired",
            Self::TooManyRequests => "upload_capacity_exceeded",
            Self::InsufficientStorage => "insufficient_storage",
            Self::Unavailable => "upload_unavailable",
        }
    }
}

#[derive(Clone)]
pub struct UploadService {
    repository: Arc<dyn UploadRepository>,
    staging: Arc<dyn UploadStagingStore>,
    limits: UploadLimits,
    leases: Arc<UploadLeaseTable>,
}

impl UploadService {
    #[must_use]
    pub fn new(
        repository: Arc<dyn UploadRepository>,
        staging: Arc<dyn UploadStagingStore>,
        mut limits: UploadLimits,
    ) -> Self {
        limits.max_chunk_size = limits.max_chunk_size.min(DEFAULT_MAX_CHUNK_SIZE);
        Self {
            repository,
            staging,
            limits,
            leases: Arc::new(UploadLeaseTable::default()),
        }
    }

    #[must_use]
    pub const fn limits(&self) -> UploadLimits {
        self.limits
    }

    pub async fn create(
        &self,
        input: NewUpload,
        now: OffsetDateTime,
    ) -> Result<UploadSession, UploadServiceError> {
        validate_new_upload(&input)?;
        self.maintain_runtime(now, None).await?;
        let available = self.staging.available_space().await.map_err(map_staging)?;
        let capacity = available
            .checked_sub(self.limits.free_space_reserve)
            .filter(|capacity| *capacity >= input.expected_size)
            .ok_or(UploadServiceError::InsufficientStorage)?;
        let expires_at = now
            .checked_add(self.limits.session_ttl)
            .ok_or(UploadServiceError::Invalid)?;
        let session = UploadSession {
            id: UploadId::new(),
            project_id: input.project_id,
            destination_parent_id: input.destination_parent_id,
            destination_name: input.destination_name,
            expected_size: input.expected_size,
            committed_offset: 0,
            expected_hash: input.expected_hash,
            pending: None,
            state: UploadState::Created,
            expires_at,
        };
        let _lease = self.leases.acquire(session.id).await;
        self.repository
            .create(&session, self.limits.max_active_sessions, capacity)
            .await
            .map_err(map_repository)?;
        if let Err(error) = self.staging.create(session.id).await {
            let staging_error = map_staging(error);
            self.repository
                .fail_pristine_created(session.id)
                .await
                .map_err(map_repository)?;
            return Err(staging_error);
        }
        let identity = match self.staging.identity(session.id).await {
            Ok(identity) => identity,
            Err(error) => {
                self.repository
                    .fail_pristine_created(session.id)
                    .await
                    .map_err(map_repository)?;
                return Err(map_staging(error));
            }
        };
        if let Some(identity) = identity
            && let Err(error) = self
                .repository
                .record_staging_identity(session.id, identity)
                .await
        {
            self.repository
                .fail_pristine_created(session.id)
                .await
                .map_err(map_repository)?;
            return Err(map_repository(error));
        }
        Ok(session)
    }

    pub async fn status(
        &self,
        id: UploadId,
        now: OffsetDateTime,
    ) -> Result<UploadSession, UploadServiceError> {
        self.maintain_runtime(now, Some(id)).await?;
        let _lease = self.leases.acquire(id).await;
        self.reconcile_locked(id, now).await
    }

    pub async fn put_chunk(
        &self,
        id: UploadId,
        offset: i64,
        bytes: &[u8],
        digest: [u8; 32],
        now: OffsetDateTime,
    ) -> Result<UploadSession, UploadServiceError> {
        let length = i64::try_from(bytes.len()).map_err(|_| UploadServiceError::Invalid)?;
        if offset < 0 || length == 0 || length > self.limits.max_chunk_size {
            return Err(UploadServiceError::Invalid);
        }
        if sha256(bytes) != digest {
            return Err(UploadServiceError::Conflict);
        }
        self.maintain_runtime(now, Some(id)).await?;
        let _lease = self.leases.acquire(id).await;
        let session = self.reconcile_locked(id, now).await?;
        let end = offset
            .checked_add(length)
            .ok_or(UploadServiceError::Conflict)?;
        if offset < session.committed_offset {
            if end > session.committed_offset {
                return Err(UploadServiceError::Conflict);
            }
            let stored = self
                .staging
                .read_exact(id, offset, length)
                .await
                .map_err(map_staging)?;
            return if sha256(&stored) == digest {
                Ok(session)
            } else {
                Err(UploadServiceError::Conflict)
            };
        }
        if offset != session.committed_offset || end > session.expected_size {
            return Err(UploadServiceError::Conflict);
        }
        let available = self.staging.available_space().await.map_err(map_staging)?;
        if available < self.limits.free_space_reserve.saturating_add(length) {
            return Err(UploadServiceError::InsufficientStorage);
        }
        self.repository
            .prepare_chunk(
                id,
                offset,
                length,
                digest,
                self.limits.max_concurrent_uploads,
            )
            .await
            .map_err(map_repository)?;
        let durable_length = self
            .staging
            .write_verify_and_flush(id, offset, bytes, digest)
            .await
            .map_err(map_staging)?;
        if durable_length < end {
            self.repository.fail(id).await.map_err(map_repository)?;
            return Err(UploadServiceError::Unavailable);
        }
        self.repository
            .commit_chunk(id, offset, length, digest)
            .await
            .map_err(map_repository)
    }

    pub async fn cancel(&self, id: UploadId) -> Result<(), UploadServiceError> {
        let _lease = self.leases.acquire(id).await;
        self.repository.cancel(id).await.map_err(map_repository)?;
        match self.staging.remove(id).await {
            Ok(()) | Err(UploadStagingError::NotFound) => Ok(()),
            Err(error) => Err(map_staging(error)),
        }
    }

    pub async fn maintain(&self, now: OffsetDateTime) -> Result<(), UploadServiceError> {
        self.maintain_runtime(now, None).await
    }

    /// Completes upload crash recovery before the service reports readiness.
    pub async fn initialize(&self, now: OffsetDateTime) -> Result<(), UploadServiceError> {
        self.initialize_all(now).await
    }

    async fn maintain_runtime(
        &self,
        now: OffsetDateTime,
        excluded: Option<UploadId>,
    ) -> Result<(), UploadServiceError> {
        for id in self.repository.active_ids().await.map_err(map_repository)? {
            if Some(id) == excluded {
                continue;
            }
            if self
                .repository
                .expire_if_due(id, now)
                .await
                .map_err(map_repository)?
            {
                let _ = self.staging.remove(id).await;
            }
        }
        self.cleanup_staging(excluded).await
    }

    async fn initialize_all(&self, now: OffsetDateTime) -> Result<(), UploadServiceError> {
        let ids = self.repository.active_ids().await.map_err(map_repository)?;
        for id in ids {
            let _lease = self.leases.acquire(id).await;
            let session = match self.repository.read(id).await {
                Ok(session) if session.state.is_active() => session,
                Ok(_) | Err(UploadRepositoryError::NotFound) => continue,
                Err(error) => return Err(map_repository(error)),
            };
            if let Err(error) = self.verify_staging_identity(id).await {
                if matches!(error, UploadServiceError::NotFound)
                    && session.state == UploadState::Created
                    && session.committed_offset == 0
                    && session.pending.is_none()
                    && self
                        .repository
                        .fail_pristine_created(id)
                        .await
                        .map_err(map_repository)?
                {
                    continue;
                }
                self.repository.fail(id).await.map_err(map_repository)?;
                continue;
            }
            if session.state == UploadState::Created
                && session.committed_offset == 0
                && session.pending.is_none()
                && matches!(
                    self.staging.length(id).await,
                    Err(UploadStagingError::NotFound)
                )
                && self
                    .repository
                    .fail_pristine_created(id)
                    .await
                    .map_err(map_repository)?
            {
                continue;
            }
            if now >= session.expires_at {
                if self
                    .repository
                    .expire_if_due(id, now)
                    .await
                    .map_err(map_repository)?
                {
                    let _ = self.staging.remove(id).await;
                }
                continue;
            }
            if let Err(error) = self.reconcile_locked(id, now).await {
                match self.repository.read(id).await {
                    Ok(session) if session.state == UploadState::Failed => continue,
                    _ => return Err(error),
                }
            }
        }
        self.cleanup_staging(None).await
    }

    async fn cleanup_staging(&self, excluded: Option<UploadId>) -> Result<(), UploadServiceError> {
        for id in self
            .repository
            .cleanup_ids()
            .await
            .map_err(map_repository)?
        {
            if Some(id) == excluded {
                continue;
            }
            let _lease = self.leases.acquire(id).await;
            if self.verify_staging_identity(id).await.is_err() {
                continue;
            }
            match self.staging.remove(id).await {
                Ok(()) | Err(UploadStagingError::NotFound) => self
                    .repository
                    .complete_cleanup(id)
                    .await
                    .map_err(map_repository)?,
                Err(_) => {}
            }
        }
        Ok(())
    }

    async fn reconcile_locked(
        &self,
        id: UploadId,
        now: OffsetDateTime,
    ) -> Result<UploadSession, UploadServiceError> {
        let mut session = self.repository.read(id).await.map_err(map_repository)?;
        if !session.state.is_active() {
            return Err(UploadServiceError::NotFound);
        }
        if let Err(error) = self.verify_staging_identity(id).await {
            self.repository.fail(id).await.map_err(map_repository)?;
            return Err(error);
        }
        if now >= session.expires_at {
            self.repository.fail(id).await.map_err(map_repository)?;
            let _ = self.staging.remove(id).await;
            return Err(UploadServiceError::Expired);
        }
        let mut durable_length = self.staging.length(id).await.map_err(map_staging)?;
        if durable_length < session.committed_offset {
            self.repository.fail(id).await.map_err(map_repository)?;
            return Err(UploadServiceError::Unavailable);
        }
        if let Some(pending) = session.pending.clone() {
            let end = pending
                .offset
                .checked_add(pending.length)
                .ok_or(UploadServiceError::Unavailable)?;
            if durable_length >= end {
                let bytes = self
                    .staging
                    .read_exact(id, pending.offset, pending.length)
                    .await
                    .map_err(map_staging)?;
                if sha256(&bytes) == pending.digest {
                    session = self
                        .repository
                        .commit_chunk(id, pending.offset, pending.length, pending.digest)
                        .await
                        .map_err(map_repository)?;
                    if durable_length > session.committed_offset {
                        self.staging
                            .truncate(id, session.committed_offset)
                            .await
                            .map_err(map_staging)?;
                    }
                    return Ok(session);
                }
            }
            self.staging
                .truncate(id, session.committed_offset)
                .await
                .map_err(map_staging)?;
            durable_length = session.committed_offset;
            session = self
                .repository
                .clear_pending(id)
                .await
                .map_err(map_repository)?;
        }
        if durable_length > session.committed_offset {
            self.staging
                .truncate(id, session.committed_offset)
                .await
                .map_err(map_staging)?;
        }
        Ok(session)
    }

    async fn verify_staging_identity(&self, id: UploadId) -> Result<(), UploadServiceError> {
        let Some(actual) = self.staging.identity(id).await.map_err(map_staging)? else {
            return Ok(());
        };
        match self
            .repository
            .staging_identity(id)
            .await
            .map_err(map_repository)?
        {
            Some(expected) if expected != actual => Err(UploadServiceError::Unavailable),
            None => self
                .repository
                .record_staging_identity(id, actual)
                .await
                .map_err(map_repository),
            Some(_) => Ok(()),
        }
    }
}

#[derive(Default)]
struct UploadLeaseTable {
    entries: StdMutex<HashMap<UploadId, Arc<Mutex<()>>>>,
}

impl UploadLeaseTable {
    async fn acquire(self: &Arc<Self>, id: UploadId) -> UploadLease {
        let lock = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Arc::clone(
                entries
                    .entry(id)
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let guard = Arc::clone(&lock).lock_owned().await;
        UploadLease {
            id,
            lock,
            guard: Some(guard),
            table: Arc::clone(self),
        }
    }
}

struct UploadLease {
    id: UploadId,
    lock: Arc<Mutex<()>>,
    guard: Option<OwnedMutexGuard<()>>,
    table: Arc<UploadLeaseTable>,
}

impl Drop for UploadLease {
    fn drop(&mut self) {
        self.guard.take();
        let mut entries = self
            .table
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if Arc::strong_count(&self.lock) == 2
            && entries
                .get(&self.id)
                .is_some_and(|lock| Arc::ptr_eq(lock, &self.lock))
        {
            entries.remove(&self.id);
        }
    }
}

fn validate_new_upload(input: &NewUpload) -> Result<(), UploadServiceError> {
    if input.expected_size < 0
        || input.destination_name.is_empty()
        || input.destination_name.len() > MAX_UPLOAD_NAME_BYTES
        || input.destination_name.contains(['/', '\\', '\0'])
        || input.destination_name == "."
        || input.destination_name == ".."
    {
        return Err(UploadServiceError::Invalid);
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn map_repository(error: UploadRepositoryError) -> UploadServiceError {
    match error {
        UploadRepositoryError::NotFound => UploadServiceError::NotFound,
        UploadRepositoryError::Conflict => UploadServiceError::Conflict,
        UploadRepositoryError::Expired => UploadServiceError::Expired,
        UploadRepositoryError::TooManySessions | UploadRepositoryError::TooManyConcurrent => {
            UploadServiceError::TooManyRequests
        }
        UploadRepositoryError::InsufficientStorage => UploadServiceError::InsufficientStorage,
        UploadRepositoryError::Unavailable => UploadServiceError::Unavailable,
    }
}

fn map_staging(error: UploadStagingError) -> UploadServiceError {
    match error {
        UploadStagingError::NotFound | UploadStagingError::Unavailable => {
            UploadServiceError::Unavailable
        }
        UploadStagingError::InsufficientStorage => UploadServiceError::InsufficientStorage,
    }
}

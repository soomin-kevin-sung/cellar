use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use sha2::{Digest as _, Sha256};
use time::{Duration, OffsetDateTime};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::{
    FileEntry, FileEntryId, InMemoryProjectMutationCoordinator, ProjectId,
    ProjectMutationCoordinator, RecoveryDecision, UploadFinalizeRepository,
    UploadFinalizeRepositoryError, UploadFinalizeStart, UploadId, UploadPublicationError,
    UploadPublisher, decide_upload_recovery,
};

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
    async fn release(&self, _id: UploadId) {}
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
    finalization: Option<Arc<FinalizationServices>>,
    project_mutations: Arc<dyn ProjectMutationCoordinator>,
}

struct FinalizationServices {
    repository: Arc<dyn UploadFinalizeRepository>,
    publisher: Arc<dyn UploadPublisher>,
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
            finalization: None,
            project_mutations: Arc::new(InMemoryProjectMutationCoordinator::default()),
        }
    }

    #[must_use]
    pub fn with_finalization(
        repository: Arc<dyn UploadRepository>,
        staging: Arc<dyn UploadStagingStore>,
        finalization_repository: Arc<dyn UploadFinalizeRepository>,
        publisher: Arc<dyn UploadPublisher>,
        limits: UploadLimits,
    ) -> Self {
        Self::with_finalization_and_coordinator(
            repository,
            staging,
            finalization_repository,
            publisher,
            limits,
            Arc::new(InMemoryProjectMutationCoordinator::default()),
        )
    }

    #[must_use]
    pub fn with_finalization_and_coordinator(
        repository: Arc<dyn UploadRepository>,
        staging: Arc<dyn UploadStagingStore>,
        finalization_repository: Arc<dyn UploadFinalizeRepository>,
        publisher: Arc<dyn UploadPublisher>,
        mut limits: UploadLimits,
        project_mutations: Arc<dyn ProjectMutationCoordinator>,
    ) -> Self {
        limits.max_chunk_size = limits.max_chunk_size.min(DEFAULT_MAX_CHUNK_SIZE);
        Self {
            repository,
            staging,
            limits,
            leases: Arc::new(UploadLeaseTable::default()),
            finalization: Some(Arc::new(FinalizationServices {
                repository: finalization_repository,
                publisher,
            })),
            project_mutations,
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
        if let Some(finalization) = &self.finalization {
            match finalization.repository.upload_commit(id).await {
                Ok(Some(UploadFinalizeStart::Intent(_)))
                | Ok(Some(UploadFinalizeStart::Completed(_))) => {
                    return self.repository.read(id).await.map_err(map_repository);
                }
                Ok(None) => {}
                Err(UploadFinalizeRepositoryError::Conflict) => {
                    return Err(UploadServiceError::NotFound);
                }
                Err(error) => return Err(map_finalize_repository(error)),
            }
        }
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
        self.reject_if_finalizing(id).await?;
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
        self.reject_if_finalizing(id).await?;
        self.repository.cancel(id).await.map_err(map_repository)?;
        match self.staging.remove(id).await {
            Ok(()) | Err(UploadStagingError::NotFound) => self
                .repository
                .complete_cleanup(id)
                .await
                .map_err(map_repository),
            Err(error) => Err(map_staging(error)),
        }
    }

    pub async fn finalize(
        &self,
        id: UploadId,
        now: OffsetDateTime,
    ) -> Result<FileEntry, UploadServiceError> {
        let finalization = self
            .finalization
            .as_ref()
            .ok_or(UploadServiceError::Unavailable)?;
        let _lease = self.leases.acquire(id).await;
        if let Some(start) = finalization
            .repository
            .upload_commit(id)
            .await
            .map_err(map_finalize_repository)?
        {
            return match start {
                UploadFinalizeStart::Completed(entry) => Ok(entry),
                UploadFinalizeStart::Intent(intent) => {
                    let _project_guard = self
                        .project_mutations
                        .project_lock(intent.project_id)
                        .lock_owned()
                        .await;
                    self.recover_commit(finalization, intent, None, now).await
                }
            };
        }

        let session = self.reconcile_locked(id, now).await?;
        let _project_guard = self
            .project_mutations
            .project_lock(session.project_id)
            .lock_owned()
            .await;
        if session.pending.is_some() || session.committed_offset != session.expected_size {
            return Err(UploadServiceError::Conflict);
        }
        let available = self.staging.available_space().await.map_err(map_staging)?;
        if available < self.limits.free_space_reserve {
            return Err(UploadServiceError::InsufficientStorage);
        }
        let target = finalization
            .repository
            .upload_finalize_target(id)
            .await
            .map_err(map_finalize_repository)?;
        let verified = finalization
            .publisher
            .verify_and_retain(id, &target, session.expected_size)
            .await
            .map_err(map_publication)?;
        let facts = verified.facts();
        if facts.size != session.expected_size
            || session
                .expected_hash
                .is_some_and(|expected| expected != facts.sha256)
        {
            return Err(UploadServiceError::Conflict);
        }
        let start = finalization
            .repository
            .prepare_upload_commit(id, &target, facts, now)
            .await
            .map_err(map_finalize_repository)?;
        match start {
            UploadFinalizeStart::Completed(entry) => Ok(entry),
            UploadFinalizeStart::Intent(intent) => {
                self.recover_commit(finalization, intent, Some(verified), now)
                    .await
            }
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
            let Some(_lease) = self.leases.try_acquire(id) else {
                continue;
            };
            if self
                .repository
                .expire_if_due(id, now)
                .await
                .map_err(map_repository)?
            {
                let _ = self.staging.remove(id).await;
            }
        }
        self.cleanup_staging(excluded, false).await
    }

    async fn initialize_all(&self, now: OffsetDateTime) -> Result<(), UploadServiceError> {
        if let Some(finalization) = &self.finalization {
            let intents = finalization
                .repository
                .pending_upload_commits()
                .await
                .map_err(map_finalize_repository)?;
            for intent in intents {
                let _lease = self.leases.acquire(intent.upload_id).await;
                let _project_guard = self
                    .project_mutations
                    .project_lock(intent.project_id)
                    .lock_owned()
                    .await;
                self.recover_commit(finalization, intent, None, now).await?;
            }
        }
        let ids = self.repository.active_ids().await.map_err(map_repository)?;
        for id in ids {
            let _lease = self.leases.acquire(id).await;
            let session = match self.repository.read(id).await {
                Ok(session) if session.state.is_active() => session,
                Ok(_) | Err(UploadRepositoryError::NotFound) => continue,
                Err(error) => return Err(map_repository(error)),
            };
            if session.state == UploadState::Committing && self.finalization.is_some() {
                return Err(UploadServiceError::Unavailable);
            }
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
        self.cleanup_staging(None, true).await
    }

    async fn cleanup_staging(
        &self,
        excluded: Option<UploadId>,
        wait_for_lease: bool,
    ) -> Result<(), UploadServiceError> {
        for id in self
            .repository
            .cleanup_ids()
            .await
            .map_err(map_repository)?
        {
            if Some(id) == excluded {
                continue;
            }
            let _lease = if wait_for_lease {
                self.leases.acquire(id).await
            } else {
                let Some(lease) = self.leases.try_acquire(id) else {
                    continue;
                };
                lease
            };
            let actual = match self.staging.identity(id).await {
                Ok(actual) => actual,
                Err(UploadStagingError::NotFound) => {
                    self.staging.release(id).await;
                    self.repository
                        .complete_cleanup(id)
                        .await
                        .map_err(map_repository)?;
                    continue;
                }
                Err(_) => {
                    self.staging.release(id).await;
                    continue;
                }
            };
            if let Some(actual) = actual {
                let expected = self
                    .repository
                    .staging_identity(id)
                    .await
                    .map_err(map_repository)?;
                if expected.is_some_and(|expected| expected != actual) {
                    self.staging.release(id).await;
                    continue;
                }
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

    async fn reject_if_finalizing(&self, id: UploadId) -> Result<(), UploadServiceError> {
        let Some(finalization) = &self.finalization else {
            return Ok(());
        };
        match finalization.repository.upload_commit(id).await {
            Ok(Some(_)) => Err(UploadServiceError::Conflict),
            Ok(None) => Ok(()),
            Err(UploadFinalizeRepositoryError::Conflict) => Err(UploadServiceError::NotFound),
            Err(error) => Err(map_finalize_repository(error)),
        }
    }

    async fn recover_commit(
        &self,
        finalization: &FinalizationServices,
        intent: crate::UploadCommitIntent,
        verified: Option<crate::VerifiedUpload>,
        now: OffsetDateTime,
    ) -> Result<FileEntry, UploadServiceError> {
        let published = if let Some(verified) = verified {
            self.validate_publication_before_publish(finalization, &intent, now)
                .await?;
            self.publish_verified(finalization, &intent, verified, now)
                .await?
        } else {
            let observation = finalization
                .publisher
                .observe(&intent)
                .await
                .map_err(map_publication)?;
            match decide_upload_recovery(observation) {
                RecoveryDecision::Publish => {
                    if intent.result_identity.is_none() {
                        self.validate_publication_before_publish(finalization, &intent, now)
                            .await?;
                    }
                    let verified = match finalization.publisher.resume_and_retain(&intent).await {
                        Ok(verified) => verified,
                        Err(UploadPublicationError::Conflict) => {
                            finalization
                                .repository
                                .fail_upload_commit(&intent, "staging_verification_mismatch", now)
                                .await
                                .map_err(map_finalize_repository)?;
                            return Err(UploadServiceError::Conflict);
                        }
                        Err(error) => return Err(map_publication(error)),
                    };
                    self.publish_verified(finalization, &intent, verified, now)
                        .await?
                }
                RecoveryDecision::CompleteCatalog => finalization
                    .publisher
                    .inspect_destination(&intent)
                    .await
                    .map_err(map_publication)?,
                RecoveryDecision::FailConflict => {
                    finalization
                        .repository
                        .fail_upload_commit(&intent, "destination_conflict", now)
                        .await
                        .map_err(map_finalize_repository)?;
                    return Err(UploadServiceError::Conflict);
                }
                RecoveryDecision::FailMissing => {
                    finalization
                        .repository
                        .fail_upload_commit(&intent, "publication_missing", now)
                        .await
                        .map_err(map_finalize_repository)?;
                    return Err(UploadServiceError::Unavailable);
                }
            }
        };
        if published.identity != intent.staging_identity || published.size != intent.expected_size {
            finalization
                .repository
                .fail_upload_commit(&intent, "publication_identity_mismatch", now)
                .await
                .map_err(map_finalize_repository)?;
            return Err(UploadServiceError::Conflict);
        }
        let intent = finalization
            .repository
            .mark_upload_fs_applied(&intent, published, now)
            .await
            .map_err(map_finalize_repository)?;
        match finalization
            .repository
            .complete_upload_commit(&intent, published, now)
            .await
        {
            Ok(entry) => Ok(entry),
            Err(UploadFinalizeRepositoryError::Conflict) => {
                finalization
                    .repository
                    .fail_upload_commit(&intent, "catalog_conflict", now)
                    .await
                    .map_err(map_finalize_repository)?;
                Err(UploadServiceError::Conflict)
            }
            Err(error) => Err(map_finalize_repository(error)),
        }
    }

    async fn validate_publication_before_publish(
        &self,
        finalization: &FinalizationServices,
        intent: &crate::UploadCommitIntent,
        now: OffsetDateTime,
    ) -> Result<(), UploadServiceError> {
        if let Err(error) = finalization
            .repository
            .validate_upload_publication(intent)
            .await
        {
            match error {
                UploadFinalizeRepositoryError::Conflict
                | UploadFinalizeRepositoryError::NotFound => {
                    finalization
                        .repository
                        .fail_upload_commit(intent, "publication_precondition_changed", now)
                        .await
                        .map_err(map_finalize_repository)?;
                    return Err(map_finalize_repository(error));
                }
                error => return Err(map_finalize_repository(error)),
            }
        }
        Ok(())
    }

    async fn publish_verified(
        &self,
        finalization: &FinalizationServices,
        intent: &crate::UploadCommitIntent,
        verified: crate::VerifiedUpload,
        now: OffsetDateTime,
    ) -> Result<crate::PublishedUpload, UploadServiceError> {
        match finalization
            .publisher
            .publish_no_replace(intent, verified)
            .await
        {
            Ok(published) => Ok(published),
            Err(UploadPublicationError::Conflict) => {
                let raced = finalization
                    .publisher
                    .observe(intent)
                    .await
                    .map_err(map_publication)?;
                match decide_upload_recovery(raced) {
                    RecoveryDecision::CompleteCatalog => finalization
                        .publisher
                        .inspect_destination(intent)
                        .await
                        .map_err(map_publication),
                    RecoveryDecision::FailConflict => {
                        finalization
                            .repository
                            .fail_upload_commit(intent, "destination_conflict", now)
                            .await
                            .map_err(map_finalize_repository)?;
                        Err(UploadServiceError::Conflict)
                    }
                    RecoveryDecision::Publish | RecoveryDecision::FailMissing => {
                        Err(UploadServiceError::Unavailable)
                    }
                }
            }
            Err(error) => Err(map_publication(error)),
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

    fn try_acquire(self: &Arc<Self>, id: UploadId) -> Option<UploadLease> {
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
        let guard = Arc::clone(&lock).try_lock_owned().ok()?;
        Some(UploadLease {
            id,
            lock,
            guard: Some(guard),
            table: Arc::clone(self),
        })
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
    if input.expected_size < 0 || !is_safe_upload_name(&input.destination_name) {
        return Err(UploadServiceError::Invalid);
    }
    Ok(())
}

#[must_use]
pub fn is_safe_upload_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_UPLOAD_NAME_BYTES
        && value.encode_utf16().count() <= 255
        && !value.contains(['/', '\\', '\0'])
        && value != "."
        && value != ".."
        && !value.ends_with(['.', ' '])
        && !value.chars().any(|character| {
            character <= '\u{1f}' || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
        })
        && !is_windows_device_name(value)
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

fn is_windows_device_name(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or(value).to_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let Some(suffix) = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
    else {
        return false;
    };
    matches!(
        suffix,
        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
    )
}

fn map_finalize_repository(error: UploadFinalizeRepositoryError) -> UploadServiceError {
    match error {
        UploadFinalizeRepositoryError::NotFound => UploadServiceError::NotFound,
        UploadFinalizeRepositoryError::Conflict => UploadServiceError::Conflict,
        UploadFinalizeRepositoryError::InsufficientStorage => {
            UploadServiceError::InsufficientStorage
        }
        UploadFinalizeRepositoryError::Unavailable => UploadServiceError::Unavailable,
    }
}

fn map_publication(error: UploadPublicationError) -> UploadServiceError {
    match error {
        UploadPublicationError::NotFound | UploadPublicationError::Unavailable => {
            UploadServiceError::Unavailable
        }
        UploadPublicationError::Conflict => UploadServiceError::Conflict,
        UploadPublicationError::InsufficientStorage => UploadServiceError::InsufficientStorage,
    }
}

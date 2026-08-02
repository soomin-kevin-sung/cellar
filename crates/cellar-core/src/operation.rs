use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::Mutex;

use crate::{FileEntry, FileEntryId, OperationId, ProjectId, StagingIdentity, UploadId};

pub const UPLOAD_COMMIT_PAYLOAD_VERSION: i64 = 2;
pub const MAX_UPLOAD_COMMIT_COMPONENTS: usize = 256;
pub const MAX_UPLOAD_COMMIT_PAYLOAD_BYTES: usize = 128 * 1024;

pub trait ProjectMutationCoordinator: Send + Sync {
    fn project_lock(&self, project_id: ProjectId) -> Arc<Mutex<()>>;
}

#[derive(Default)]
pub struct InMemoryProjectMutationCoordinator {
    locks: StdMutex<HashMap<ProjectId, Arc<Mutex<()>>>>,
}

impl ProjectMutationCoordinator for InMemoryProjectMutationCoordinator {
    fn project_lock(&self, project_id: ProjectId) -> Arc<Mutex<()>> {
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks
            .entry(project_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationPresence {
    Absent,
    Expected,
    Unexpected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UploadPublicationObservation {
    pub staging: PublicationPresence,
    pub destination: PublicationPresence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryDecision {
    Publish,
    CompleteCatalog,
    FailConflict,
    FailMissing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NamespaceMutationObservation {
    pub source: PublicationPresence,
    pub destination: PublicationPresence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NamespaceMutationRecoveryDecision {
    Retry,
    Complete,
    Conflict,
    Missing,
}

#[must_use]
pub const fn decide_namespace_mutation_recovery(
    observation: NamespaceMutationObservation,
) -> NamespaceMutationRecoveryDecision {
    use NamespaceMutationRecoveryDecision::{Complete, Conflict, Missing, Retry};
    use PublicationPresence::{Absent, Expected};
    match (observation.source, observation.destination) {
        (Expected, Absent) => Retry,
        (Absent, Expected) => Complete,
        (Absent, Absent) => Missing,
        _ => Conflict,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CopyMutationObservation {
    pub source: PublicationPresence,
    pub staging: PublicationPresence,
    pub destination: PublicationPresence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CopyMutationRecoveryDecision {
    Publish,
    Complete,
    Recopy,
    Conflict,
    Missing,
}

#[must_use]
pub const fn decide_copy_mutation_recovery(
    observation: CopyMutationObservation,
) -> CopyMutationRecoveryDecision {
    use CopyMutationRecoveryDecision::{Complete, Conflict, Missing, Publish, Recopy};
    use PublicationPresence::{Absent, Expected, Unexpected};
    if matches!(observation.source, Unexpected)
        || matches!(observation.staging, Unexpected)
        || matches!(observation.destination, Unexpected)
    {
        return Conflict;
    }
    match (
        observation.source,
        observation.staging,
        observation.destination,
    ) {
        (_, Expected, Absent) => Publish,
        (_, Absent, Expected) => Complete,
        (Expected, Absent, Absent) => Recopy,
        (Absent, Absent, Absent) => Missing,
        _ => Conflict,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaseRenameObservation {
    pub source: PublicationPresence,
    pub temporary: PublicationPresence,
    pub final_name: PublicationPresence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaseRenameRecoveryDecision {
    RenameToTemporary,
    RenameToFinal,
    Complete,
    Conflict,
    Missing,
}

#[must_use]
pub const fn decide_case_rename_recovery(
    observation: CaseRenameObservation,
) -> CaseRenameRecoveryDecision {
    use CaseRenameRecoveryDecision::{
        Complete, Conflict, Missing, RenameToFinal, RenameToTemporary,
    };
    use PublicationPresence::{Absent, Expected, Unexpected};
    if matches!(observation.source, Unexpected)
        || matches!(observation.temporary, Unexpected)
        || matches!(observation.final_name, Unexpected)
    {
        return Conflict;
    }
    match (
        observation.source,
        observation.temporary,
        observation.final_name,
    ) {
        (Expected, Absent, Absent) => RenameToTemporary,
        (Absent, Expected, Absent) => RenameToFinal,
        (Absent, Absent, Expected) => Complete,
        (Absent, Absent, Absent) => Missing,
        _ => Conflict,
    }
}

#[must_use]
pub const fn decide_upload_recovery(observation: UploadPublicationObservation) -> RecoveryDecision {
    use PublicationPresence::{Absent, Expected};
    match (observation.staging, observation.destination) {
        (Expected, Absent) => RecoveryDecision::Publish,
        (Absent, Expected) => RecoveryDecision::CompleteCatalog,
        (Absent, Absent) => RecoveryDecision::FailMissing,
        _ => RecoveryDecision::FailConflict,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UploadCommitPayload {
    pub upload_id: String,
    pub project_id: String,
    pub destination_parent_id: Option<String>,
    pub destination_parent_revision: Option<String>,
    pub destination_parent_identity: Option<String>,
    pub destination_namespace_identity: String,
    pub destination_components: Vec<String>,
    pub destination_name: String,
    pub file_entry_id: String,
    pub expected_size: String,
    pub sha256: String,
    pub staging_identity: String,
    pub result_identity: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadCommitIntent {
    pub operation_id: OperationId,
    pub upload_id: UploadId,
    pub project_id: ProjectId,
    pub destination_parent_id: Option<FileEntryId>,
    pub destination_parent_revision: Option<i64>,
    pub destination_parent_identity: Option<StagingIdentity>,
    /// Identity of the actual directory handle receiving the rename. This is
    /// present for root-level and child destinations alike.
    pub destination_namespace_identity: StagingIdentity,
    /// Exact, validated path components below `projects/<id>/files`.
    pub destination_components: Vec<String>,
    pub destination_name: String,
    pub file_entry_id: FileEntryId,
    pub expected_size: i64,
    pub sha256: [u8; 32],
    pub staging_identity: StagingIdentity,
    pub result_identity: Option<StagingIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadFinalizeTarget {
    pub project_id: ProjectId,
    pub destination_parent_id: Option<FileEntryId>,
    pub destination_parent_revision: Option<i64>,
    pub destination_parent_identity: Option<StagingIdentity>,
    pub destination_components: Vec<String>,
    pub destination_name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifiedUploadFacts {
    pub size: i64,
    pub sha256: [u8; 32],
    pub staging_identity: StagingIdentity,
    pub destination_namespace_identity: StagingIdentity,
}

pub struct VerifiedUpload {
    facts: VerifiedUploadFacts,
    token: Box<dyn Any + Send>,
}

impl VerifiedUpload {
    #[must_use]
    pub fn new<T: Any + Send>(facts: VerifiedUploadFacts, token: T) -> Self {
        Self {
            facts,
            token: Box::new(token),
        }
    }

    #[must_use]
    pub const fn facts(&self) -> VerifiedUploadFacts {
        self.facts
    }

    #[must_use]
    pub fn into_token<T: Any + Send>(self) -> Option<T> {
        self.token.downcast::<T>().ok().map(|token| *token)
    }
}

impl std::fmt::Debug for VerifiedUpload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VerifiedUpload")
            .field("facts", &self.facts)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublishedUpload {
    pub identity: StagingIdentity,
    pub size: i64,
    pub mtime_filetime_100ns: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UploadFinalizeStart {
    Intent(UploadCommitIntent),
    Completed(FileEntry),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UploadFinalizeRepositoryError {
    NotFound,
    Conflict,
    InsufficientStorage,
    Unavailable,
}

#[async_trait]
pub trait UploadFinalizeRepository: Send + Sync {
    async fn upload_commit(
        &self,
        upload_id: UploadId,
    ) -> Result<Option<UploadFinalizeStart>, UploadFinalizeRepositoryError>;

    async fn upload_finalize_target(
        &self,
        upload_id: UploadId,
    ) -> Result<UploadFinalizeTarget, UploadFinalizeRepositoryError>;

    async fn prepare_upload_commit(
        &self,
        upload_id: UploadId,
        target: &UploadFinalizeTarget,
        verified: VerifiedUploadFacts,
        now: OffsetDateTime,
    ) -> Result<UploadFinalizeStart, UploadFinalizeRepositoryError>;

    /// Revalidates mutable project and destination-parent publication
    /// preconditions. Call only while no filesystem publication has occurred.
    async fn validate_upload_publication(
        &self,
        intent: &UploadCommitIntent,
    ) -> Result<(), UploadFinalizeRepositoryError>;

    async fn mark_upload_fs_applied(
        &self,
        intent: &UploadCommitIntent,
        published: PublishedUpload,
        now: OffsetDateTime,
    ) -> Result<UploadCommitIntent, UploadFinalizeRepositoryError>;

    async fn complete_upload_commit(
        &self,
        intent: &UploadCommitIntent,
        published: PublishedUpload,
        now: OffsetDateTime,
    ) -> Result<FileEntry, UploadFinalizeRepositoryError>;

    async fn pending_upload_commits(
        &self,
    ) -> Result<Vec<UploadCommitIntent>, UploadFinalizeRepositoryError>;

    async fn fail_upload_commit(
        &self,
        intent: &UploadCommitIntent,
        error_code: &'static str,
        now: OffsetDateTime,
    ) -> Result<(), UploadFinalizeRepositoryError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UploadPublicationError {
    NotFound,
    Conflict,
    InsufficientStorage,
    Unavailable,
}

#[async_trait]
pub trait UploadPublisher: Send + Sync {
    /// Verifies the durable staging bytes and retains the exact source and
    /// destination namespace handles until publication or cancellation.
    async fn verify_and_retain(
        &self,
        upload_id: UploadId,
        target: &UploadFinalizeTarget,
        expected_size: i64,
    ) -> Result<VerifiedUpload, UploadPublicationError>;

    /// Reopens a crash-recovery source with exclusive write/delete sharing,
    /// revalidates every persisted fact, and retains that exact handle.
    async fn resume_and_retain(
        &self,
        intent: &UploadCommitIntent,
    ) -> Result<VerifiedUpload, UploadPublicationError>;

    async fn observe(
        &self,
        intent: &UploadCommitIntent,
    ) -> Result<UploadPublicationObservation, UploadPublicationError>;

    /// Publishes with atomic no-replace semantics and verifies the destination identity.
    async fn publish_no_replace(
        &self,
        intent: &UploadCommitIntent,
        verified: VerifiedUpload,
    ) -> Result<PublishedUpload, UploadPublicationError>;

    async fn inspect_destination(
        &self,
        intent: &UploadCommitIntent,
    ) -> Result<PublishedUpload, UploadPublicationError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn project_mutation_coordinator_serializes_only_the_same_project() {
        let coordinator = InMemoryProjectMutationCoordinator::default();
        let first_project = ProjectId::new();
        let other_project = ProjectId::new();
        let first_lock = coordinator.project_lock(first_project);
        let same_lock = coordinator.project_lock(first_project);
        let other_lock = coordinator.project_lock(other_project);

        let guard = first_lock.lock_owned().await;
        assert!(same_lock.try_lock().is_err());
        assert!(other_lock.try_lock().is_ok());
        drop(guard);
        assert!(coordinator.project_lock(first_project).try_lock().is_ok());
    }
}

use std::sync::Arc;

use cellar_api::health::Readiness;
use cellar_core::{
    InMemoryProjectMutationCoordinator, ProjectMutationCoordinator, ReadinessBlocker, UploadLimits,
    UploadPublisher, UploadService, UploadStagingStore,
};
use cellar_db::{SqliteOperationRepository, SqliteUploadRepository};
use time::OffsetDateTime;

use crate::app::AppError;
use crate::file_mutations::ProductionFileMutationSource;

/// Recovers journaled file namespace mutations before the origin listener is bound.
pub async fn initialize_file_mutation_recovery(
    source: &ProductionFileMutationSource,
) -> Result<(), AppError> {
    source
        .recover_pending()
        .await
        .map_err(|_| AppError::Recovery)
}

/// Recovers both resumable staging and journaled final publications before
/// clearing the readiness recovery gate.
pub async fn initialize_upload_finalization_recovery(
    pool: &sqlx::SqlitePool,
    staging: Arc<dyn UploadStagingStore>,
    publisher: Arc<dyn UploadPublisher>,
    readiness: &Readiness,
    now: OffsetDateTime,
) -> Result<UploadService, AppError> {
    initialize_upload_finalization_recovery_with_coordinator(
        pool,
        staging,
        publisher,
        readiness,
        now,
        Arc::new(InMemoryProjectMutationCoordinator::default()),
    )
    .await
}

pub async fn initialize_upload_finalization_recovery_with_coordinator(
    pool: &sqlx::SqlitePool,
    staging: Arc<dyn UploadStagingStore>,
    publisher: Arc<dyn UploadPublisher>,
    readiness: &Readiness,
    now: OffsetDateTime,
    project_mutations: Arc<dyn ProjectMutationCoordinator>,
) -> Result<UploadService, AppError> {
    let uploads = Arc::new(SqliteUploadRepository::new(pool.clone()));
    let operations = Arc::new(SqliteOperationRepository::new(pool.clone()));
    let service = UploadService::with_finalization_and_coordinator(
        uploads,
        staging,
        operations,
        publisher,
        UploadLimits::default(),
        project_mutations,
    );
    service
        .initialize(now)
        .await
        .map_err(|_| AppError::Recovery)?;
    readiness.clear(ReadinessBlocker::RecoveryRequired);
    Ok(service)
}

use std::sync::Arc;

use cellar_api::health::Readiness;
use cellar_core::{
    ReadinessBlocker, UploadLimits, UploadPublisher, UploadService, UploadStagingStore,
};
use cellar_db::{SqliteOperationRepository, SqliteUploadRepository};
use time::OffsetDateTime;

use crate::app::AppError;

/// Recovers both resumable staging and journaled final publications before
/// clearing the readiness recovery gate.
pub async fn initialize_upload_finalization_recovery(
    pool: &sqlx::SqlitePool,
    staging: Arc<dyn UploadStagingStore>,
    publisher: Arc<dyn UploadPublisher>,
    readiness: &Readiness,
    now: OffsetDateTime,
) -> Result<UploadService, AppError> {
    let uploads = Arc::new(SqliteUploadRepository::new(pool.clone()));
    let operations = Arc::new(SqliteOperationRepository::new(pool.clone()));
    let service = UploadService::with_finalization(
        uploads,
        staging,
        operations,
        publisher,
        UploadLimits::default(),
    );
    service
        .initialize(now)
        .await
        .map_err(|_| AppError::Recovery)?;
    readiness.clear(ReadinessBlocker::RecoveryRequired);
    Ok(service)
}

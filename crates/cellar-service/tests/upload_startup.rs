#![cfg(windows)]

use std::sync::Arc;

use cellar_api::health::Readiness;
use cellar_core::{
    NewUpload, ProjectId, ReadinessBlocker, UploadId, UploadRepository, UploadStagingError,
    UploadStagingStore, UploadState,
};
use cellar_db::{FilenameCollation, SqliteUploadRepository};
use cellar_service::app::{check_startup_gates, initialize_upload_recovery};
use cellar_windows::{WindowsStorage, WindowsUploadStaging};
use sha2::{Digest as _, Sha256};
use tempfile::{TempDir, tempdir};
use time::{Duration, OffsetDateTime};

struct Harness {
    _database_directory: TempDir,
    storage_directory: TempDir,
    pool: sqlx::SqlitePool,
    staging: Option<Arc<WindowsUploadStaging>>,
    repository: Arc<SqliteUploadRepository>,
    project_id: ProjectId,
    now: OffsetDateTime,
}

impl Harness {
    async fn new() -> Self {
        let database_directory = tempdir().unwrap();
        let storage_directory = tempdir().unwrap();
        let pool = cellar_db::open_pool(
            &database_directory.path().join("cellar.db"),
            FilenameCollation::windows_ordinal_ci_v1(str::cmp),
        )
        .await
        .unwrap();
        cellar_db::migrate(&pool).await.unwrap();
        let project_id = ProjectId::new();
        sqlx::query(
            "INSERT INTO project
             (id, name, status, version, created_at, updated_at)
             VALUES (?, 'restart', 'active', 1, '2026-08-02T00:00:00.000000000Z',
                     '2026-08-02T00:00:00.000000000Z')",
        )
        .bind(project_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
        let identity = cellar_windows::preflight::open_as_service(storage_directory.path())
            .expect("startup storage open");
        let storage = WindowsStorage::adopt(identity).expect("adopt trusted root");
        let staging = Arc::new(
            WindowsUploadStaging::open(storage).expect("open secure upload staging directory"),
        );
        let repository = Arc::new(SqliteUploadRepository::new(pool.clone()));
        Self {
            _database_directory: database_directory,
            storage_directory,
            pool,
            staging: Some(staging),
            repository,
            project_id,
            now: OffsetDateTime::from_unix_timestamp(1_780_000_000).unwrap(),
        }
    }

    async fn create(&self) -> (cellar_core::UploadService, UploadId) {
        let readiness = Readiness::new([ReadinessBlocker::RecoveryRequired]);
        let service = initialize_upload_recovery(&self.pool, self.staging(), &readiness, self.now)
            .await
            .unwrap();
        let session = service
            .create(
                NewUpload {
                    project_id: self.project_id,
                    destination_parent_id: None,
                    destination_name: "restart.bin".into(),
                    expected_size: 32,
                    expected_hash: None,
                },
                self.now,
            )
            .await
            .unwrap();
        (service, session.id)
    }

    async fn restart(&mut self) -> (cellar_core::UploadService, Readiness) {
        drop(self.staging.take());
        let staging = self.reopen_staging();
        self.staging = Some(staging.clone());
        let readiness = Readiness::new([ReadinessBlocker::RecoveryRequired]);
        let service = initialize_upload_recovery(&self.pool, staging, &readiness, self.now)
            .await
            .unwrap();
        (service, readiness)
    }

    fn reopen_staging(&self) -> Arc<WindowsUploadStaging> {
        let identity = cellar_windows::preflight::open_as_service(self.storage_directory.path())
            .expect("reopen installed storage root");
        let storage = WindowsStorage::adopt(identity).expect("readopt trusted root");
        Arc::new(WindowsUploadStaging::open(storage).expect("reopen upload staging"))
    }

    fn staging(&self) -> Arc<WindowsUploadStaging> {
        self.staging.as_ref().expect("staging is open").clone()
    }
}

#[tokio::test]
async fn restart_commits_a_matching_durable_pending_chunk_before_readiness() {
    let mut harness = Harness::new().await;
    let (service, id) = harness.create().await;
    let digest: [u8; 32] = Sha256::digest(b"abc").into();
    harness
        .repository
        .prepare_chunk(id, 0, 3, digest, 3)
        .await
        .unwrap();
    harness
        .staging()
        .write_exact_and_flush(id, 0, b"abc")
        .await
        .unwrap();

    drop(service);
    let (restarted, readiness) = harness.restart().await;

    let recovered = restarted.status(id, harness.now).await.unwrap();
    assert_eq!(recovered.committed_offset, 3);
    assert!(recovered.pending.is_none());
    assert!(!readiness.blocker_codes().contains(&"recovery_required"));
}

#[tokio::test]
async fn restart_truncates_an_uncommitted_crash_tail_before_readiness() {
    let mut harness = Harness::new().await;
    let (service, id) = harness.create().await;
    let digest: [u8; 32] = Sha256::digest(b"ab").into();
    service
        .put_chunk(id, 0, b"ab", digest, harness.now)
        .await
        .unwrap();
    harness
        .staging()
        .write_exact_and_flush(id, 2, b"TAIL")
        .await
        .unwrap();

    drop(service);
    let (_restarted, readiness) = harness.restart().await;

    assert_eq!(harness.staging().length(id).await.unwrap(), 2);
    assert!(!readiness.blocker_codes().contains(&"recovery_required"));
}

#[tokio::test]
async fn restart_marks_short_durable_upload_failed_and_finishes_recovery() {
    let mut harness = Harness::new().await;
    let (service, id) = harness.create().await;
    let digest: [u8; 32] = Sha256::digest(b"abc").into();
    service
        .put_chunk(id, 0, b"abc", digest, harness.now)
        .await
        .unwrap();
    harness.staging().truncate(id, 2).await.unwrap();

    drop(service);
    let (_restarted, readiness) = harness.restart().await;

    let failed = harness.repository.read(id).await.unwrap();
    assert_eq!(failed.state, UploadState::Failed);
    assert!(!readiness.blocker_codes().contains(&"recovery_required"));
}

#[tokio::test]
async fn healthy_active_upload_remains_resumable_and_does_not_block_startup() {
    let mut harness = Harness::new().await;
    let (service, id) = harness.create().await;

    drop(service);
    let (restarted, readiness) = harness.restart().await;
    let gates = check_startup_gates(&harness.pool).await.unwrap();

    assert_eq!(
        restarted.status(id, harness.now).await.unwrap().state,
        UploadState::Created
    );
    assert!(gates.recovery_complete);
    assert!(gates.reconciliation_complete);
    assert!(!readiness.blocker_codes().contains(&"recovery_required"));
}

struct FailingStaging;

#[async_trait::async_trait]
impl UploadStagingStore for FailingStaging {
    async fn create(&self, _id: UploadId) -> Result<(), UploadStagingError> {
        Err(UploadStagingError::Unavailable)
    }
    async fn length(&self, _id: UploadId) -> Result<i64, UploadStagingError> {
        Err(UploadStagingError::Unavailable)
    }
    async fn read_exact(
        &self,
        _id: UploadId,
        _offset: i64,
        _length: i64,
    ) -> Result<Vec<u8>, UploadStagingError> {
        Err(UploadStagingError::Unavailable)
    }
    async fn truncate(&self, _id: UploadId, _length: i64) -> Result<(), UploadStagingError> {
        Err(UploadStagingError::Unavailable)
    }
    async fn write_exact_and_flush(
        &self,
        _id: UploadId,
        _offset: i64,
        _bytes: &[u8],
    ) -> Result<(), UploadStagingError> {
        Err(UploadStagingError::Unavailable)
    }
    async fn remove(&self, _id: UploadId) -> Result<(), UploadStagingError> {
        Err(UploadStagingError::Unavailable)
    }
    async fn available_space(&self) -> Result<i64, UploadStagingError> {
        Err(UploadStagingError::Unavailable)
    }
}

#[tokio::test]
async fn initialization_failure_keeps_readiness_blocked_with_sanitized_reason() {
    let harness = Harness::new().await;
    let (_service, _id) = harness.create().await;
    let readiness = Readiness::new([ReadinessBlocker::RecoveryRequired]);

    let result = initialize_upload_recovery(
        &harness.pool,
        Arc::new(FailingStaging),
        &readiness,
        harness.now + Duration::seconds(1),
    )
    .await;
    let error = match result {
        Ok(_) => panic!("failing staging unexpectedly initialized"),
        Err(error) => error,
    };

    assert_eq!(error.code(), "startup_recovery_failed");
    assert_eq!(error.to_string(), "startup_recovery_failed");
    assert!(readiness.blocker_codes().contains(&"recovery_required"));
}

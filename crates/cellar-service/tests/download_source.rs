use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cellar_api::routes::files::{DownloadError, DownloadSource, DownloadSpan};
use cellar_core::{FileEntryId, ProjectId};
use cellar_db::{FilenameCollation, migrate, open_pool};
use cellar_service::downloads::{
    BoundedReconciliationScheduler, DownloadPlatform, PlatformDownloadHandle, PlatformError,
    PlatformFacts, ProductionDownloadSource, ReconciliationRequest, ReconciliationScheduler,
    SqliteDownloadCatalog,
};
use sqlx::SqlitePool;
use tempfile::TempDir;

#[derive(Clone)]
struct FakePlatform {
    initial: PlatformFacts,
    verify: PlatformFacts,
    opened: Arc<Mutex<Vec<Vec<String>>>>,
    bytes: Arc<Vec<u8>>,
}

#[async_trait]
impl DownloadPlatform for FakePlatform {
    async fn open_read_exclusive(
        &self,
        components: &[String],
    ) -> Result<Box<dyn PlatformDownloadHandle>, PlatformError> {
        self.opened.lock().unwrap().push(components.to_vec());
        Ok(Box::new(FakeHandle {
            initial: self.initial,
            verify: self.verify,
            bytes: Arc::clone(&self.bytes),
        }))
    }
}

struct FakeHandle {
    initial: PlatformFacts,
    verify: PlatformFacts,
    bytes: Arc<Vec<u8>>,
}

#[async_trait]
impl PlatformDownloadHandle for FakeHandle {
    fn facts(&self) -> PlatformFacts {
        self.initial
    }

    async fn verify(&self) -> Result<PlatformFacts, PlatformError> {
        Ok(self.verify)
    }

    async fn read_exact_chunk(&mut self, span: DownloadSpan) -> Result<Vec<u8>, PlatformError> {
        let start = span.start() as usize;
        let end = start + span.length() as usize;
        Ok(self.bytes[start..end].to_vec())
    }
}

#[derive(Default)]
struct RecordingScheduler(Mutex<Vec<ReconciliationRequest>>);

impl ReconciliationScheduler for RecordingScheduler {
    fn try_schedule(&self, request: ReconciliationRequest) -> bool {
        self.0.lock().unwrap().push(request);
        true
    }
}

struct Fixture {
    _directory: TempDir,
    pool: SqlitePool,
    project: ProjectId,
    file: FileEntryId,
    facts: PlatformFacts,
}

impl Fixture {
    async fn new(project_status: &str, deleted: bool, file_state: &str) -> Self {
        let directory = TempDir::new().unwrap();
        let pool = open_pool(
            directory.path().join("cellar.db"),
            FilenameCollation::windows_ordinal_ci_v1(|left, right| left.cmp(right)),
        )
        .await
        .unwrap();
        migrate(&pool).await.unwrap();
        let project = ProjectId::new();
        let file = FileEntryId::new();
        sqlx::query(
            "INSERT INTO project
             (id, name, description, status, version, created_at, updated_at, deleted_at)
             VALUES (?, 'Project', '', ?, 1, '2026-07-31T00:00:00Z',
                     '2026-07-31T00:00:00Z', ?)",
        )
        .bind(project.to_string())
        .bind(project_status)
        .bind(deleted.then_some("2026-08-01T00:00:00Z"))
        .execute(&pool)
        .await
        .unwrap();
        let volume_serial = 77_u64;
        let file_id = 99_u128;
        sqlx::query(
            "INSERT INTO file_entry
             (id, project_id, parent_id, exact_name, kind, platform_kind,
              volume_serial, filesystem_file_id, size, mtime_filetime_100ns,
              hash, hash_state, state, revision, scan_generation, observed_at)
             VALUES (?, ?, NULL, 'payload.bin', 'file', 'windows_file_id', ?, ?,
                     6, 1234, ?, 'ready', ?, 1, 1, '2026-07-31T00:00:00Z')",
        )
        .bind(file.to_string())
        .bind(project.to_string())
        .bind(volume_serial.to_le_bytes().as_slice())
        .bind(file_id.to_le_bytes().as_slice())
        .bind([0xab_u8; 32].as_slice())
        .bind(file_state)
        .execute(&pool)
        .await
        .unwrap();
        Self {
            _directory: directory,
            pool,
            project,
            file,
            facts: PlatformFacts {
                volume_serial,
                file_id,
                length: 6,
                mtime_filetime_100ns: 1234,
            },
        }
    }

    fn source(
        &self,
        initial: PlatformFacts,
        verify: PlatformFacts,
        scheduler: Arc<RecordingScheduler>,
    ) -> (ProductionDownloadSource, Arc<Mutex<Vec<Vec<String>>>>) {
        let opened = Arc::new(Mutex::new(Vec::new()));
        let platform = Arc::new(FakePlatform {
            initial,
            verify,
            opened: Arc::clone(&opened),
            bytes: Arc::new(b"abcdef".to_vec()),
        });
        (
            ProductionDownloadSource::new(
                Arc::new(SqliteDownloadCatalog::new(self.pool.clone())),
                platform,
                scheduler,
            ),
            opened,
        )
    }
}

#[tokio::test]
async fn archived_project_opens_exact_handle_relative_components_and_ready_hash() {
    let fixture = Fixture::new("archived", false, "live").await;
    let scheduler = Arc::new(RecordingScheduler::default());
    let (source, opened) = fixture.source(fixture.facts, fixture.facts, scheduler);
    let mut download = source
        .open_verified(fixture.project, fixture.file)
        .await
        .unwrap();
    assert_eq!(download.metadata().filename(), "payload.bin");
    assert_eq!(download.metadata().length(), 6);
    assert_eq!(download.metadata().ready_sha256(), Some([0xab; 32]));
    assert_eq!(
        opened.lock().unwrap()[0],
        [
            "projects".to_owned(),
            fixture.project.to_string(),
            "files".to_owned(),
            "payload.bin".to_owned(),
        ]
    );
    assert_eq!(download.verify().await, Ok(()));
    assert_eq!(
        download
            .read_exact_chunk(DownloadSpan::new(1, 3).unwrap())
            .await,
        Ok(b"bcd".to_vec())
    );
    fixture.pool.close().await;
}

#[tokio::test]
async fn non_ready_hash_is_never_exposed_as_a_strong_etag() {
    let fixture = Fixture::new("active", false, "live").await;
    sqlx::query("UPDATE file_entry SET hash = NULL, hash_state = 'computing' WHERE id = ?")
        .bind(fixture.file.to_string())
        .execute(&fixture.pool)
        .await
        .unwrap();
    let (source, _) = fixture.source(
        fixture.facts,
        fixture.facts,
        Arc::new(RecordingScheduler::default()),
    );
    let download = source
        .open_verified(fixture.project, fixture.file)
        .await
        .unwrap();
    assert_eq!(download.metadata().ready_sha256(), None);
    fixture.pool.close().await;
}

#[tokio::test]
async fn reconciliation_scheduler_is_bounded_without_evicting_queued_work() {
    let (scheduler, mut receiver) = BoundedReconciliationScheduler::channel(1);
    let first = ReconciliationRequest {
        project_id: ProjectId::new(),
        file_id: FileEntryId::new(),
    };
    let second = ReconciliationRequest {
        project_id: ProjectId::new(),
        file_id: FileEntryId::new(),
    };
    assert!(scheduler.try_schedule(first));
    assert!(!scheduler.try_schedule(second));
    assert_eq!(receiver.recv().await, Some(first));
}

#[tokio::test]
async fn deleted_missing_settling_and_unsupported_catalog_states_fail_closed() {
    for (status, deleted, state, expected) in [
        ("active", true, "live", DownloadError::ProjectNotFound),
        ("active", false, "missing", DownloadError::FileNotFound),
        ("active", false, "trashed", DownloadError::FileNotFound),
        ("active", false, "settling", DownloadError::Settling),
        ("active", false, "unsupported", DownloadError::Unsupported),
    ] {
        let fixture = Fixture::new(status, deleted, state).await;
        let (source, opened) = fixture.source(
            fixture.facts,
            fixture.facts,
            Arc::new(RecordingScheduler::default()),
        );
        assert!(matches!(
            source.open_verified(fixture.project, fixture.file).await,
            Err(error) if error == expected
        ));
        assert!(opened.lock().unwrap().is_empty());
        fixture.pool.close().await;
    }
}

#[tokio::test]
async fn identity_size_and_mtime_mismatches_schedule_reconciliation_before_conflict() {
    let fixture = Fixture::new("active", false, "live").await;
    for mismatched in [
        PlatformFacts {
            file_id: fixture.facts.file_id + 1,
            ..fixture.facts
        },
        PlatformFacts {
            length: fixture.facts.length + 1,
            ..fixture.facts
        },
        PlatformFacts {
            mtime_filetime_100ns: fixture.facts.mtime_filetime_100ns + 1,
            ..fixture.facts
        },
    ] {
        let scheduler = Arc::new(RecordingScheduler::default());
        let (source, _) = fixture.source(mismatched, mismatched, Arc::clone(&scheduler));
        assert!(matches!(
            source.open_verified(fixture.project, fixture.file).await,
            Err(DownloadError::IdentityChanged)
        ));
        assert_eq!(scheduler.0.lock().unwrap().len(), 1);
    }

    let scheduler = Arc::new(RecordingScheduler::default());
    let mut verify_race = fixture.facts;
    verify_race.mtime_filetime_100ns += 1;
    let (source, _) = fixture.source(fixture.facts, verify_race, Arc::clone(&scheduler));
    let download = source
        .open_verified(fixture.project, fixture.file)
        .await
        .unwrap();
    assert_eq!(download.verify().await, Err(DownloadError::IdentityChanged));
    assert_eq!(scheduler.0.lock().unwrap().len(), 1);
    fixture.pool.close().await;
}

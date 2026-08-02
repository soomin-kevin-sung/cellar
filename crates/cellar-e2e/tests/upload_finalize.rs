use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cellar_core::{
    NewUpload, PublicationPresence, PublishedUpload, StagingIdentity, UploadCommitIntent,
    UploadFinalizeRepository, UploadFinalizeStart, UploadId, UploadLimits, UploadPublicationError,
    UploadPublicationObservation, UploadPublisher, UploadService, UploadStagingError,
    UploadStagingStore, VerifiedUpload,
};
use cellar_db::{
    FilenameCollation, SqliteOperationRepository, SqliteUploadRepository, migrate, open_pool,
};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};

#[derive(Clone, Copy, Debug)]
enum FaultPoint {
    Intent,
    Rename,
    CatalogCommit,
}

#[derive(Clone)]
struct Destination {
    identity: StagingIdentity,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct DurableNamespace {
    staging: Mutex<HashMap<UploadId, (StagingIdentity, Vec<u8>)>>,
    destinations: Mutex<HashMap<String, Destination>>,
}

impl DurableNamespace {
    fn identity(id: UploadId) -> StagingIdentity {
        let digest = Sha256::digest(id.to_string().as_bytes());
        let mut bytes = [0_u8; 24];
        bytes.copy_from_slice(&digest[..24]);
        StagingIdentity::new(bytes)
    }

    fn key(intent: &UploadCommitIntent) -> String {
        format!(
            "{}/{}/{}",
            intent.project_id,
            intent.destination_components.join("/"),
            intent.destination_name
        )
    }
}

#[async_trait]
impl UploadStagingStore for DurableNamespace {
    async fn create(&self, id: UploadId) -> Result<(), UploadStagingError> {
        if self
            .staging
            .lock()
            .unwrap()
            .insert(id, (Self::identity(id), Vec::new()))
            .is_some()
        {
            return Err(UploadStagingError::Unavailable);
        }
        Ok(())
    }

    async fn length(&self, id: UploadId) -> Result<i64, UploadStagingError> {
        self.staging
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|(_, bytes)| i64::try_from(bytes.len()).ok())
            .ok_or(UploadStagingError::NotFound)
    }

    async fn read_exact(
        &self,
        id: UploadId,
        offset: i64,
        length: i64,
    ) -> Result<Vec<u8>, UploadStagingError> {
        let start = usize::try_from(offset).map_err(|_| UploadStagingError::Unavailable)?;
        let end = start
            .checked_add(usize::try_from(length).map_err(|_| UploadStagingError::Unavailable)?)
            .ok_or(UploadStagingError::Unavailable)?;
        self.staging
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|(_, bytes)| bytes.get(start..end))
            .map(<[u8]>::to_vec)
            .ok_or(UploadStagingError::Unavailable)
    }

    async fn truncate(&self, id: UploadId, length: i64) -> Result<(), UploadStagingError> {
        let length = usize::try_from(length).map_err(|_| UploadStagingError::Unavailable)?;
        let mut staging = self.staging.lock().unwrap();
        staging
            .get_mut(&id)
            .ok_or(UploadStagingError::NotFound)?
            .1
            .truncate(length);
        Ok(())
    }

    async fn write_exact_and_flush(
        &self,
        id: UploadId,
        offset: i64,
        input: &[u8],
    ) -> Result<(), UploadStagingError> {
        let offset = usize::try_from(offset).map_err(|_| UploadStagingError::Unavailable)?;
        let mut staging = self.staging.lock().unwrap();
        let bytes = &mut staging.get_mut(&id).ok_or(UploadStagingError::NotFound)?.1;
        if bytes.len() != offset {
            return Err(UploadStagingError::Unavailable);
        }
        bytes.extend_from_slice(input);
        Ok(())
    }

    async fn remove(&self, id: UploadId) -> Result<(), UploadStagingError> {
        self.staging
            .lock()
            .unwrap()
            .remove(&id)
            .map(|_| ())
            .ok_or(UploadStagingError::NotFound)
    }

    async fn available_space(&self) -> Result<i64, UploadStagingError> {
        Ok(i64::MAX)
    }

    async fn identity(&self, id: UploadId) -> Result<Option<StagingIdentity>, UploadStagingError> {
        self.staging
            .lock()
            .unwrap()
            .get(&id)
            .map(|(identity, _)| Some(*identity))
            .ok_or(UploadStagingError::NotFound)
    }
}

#[async_trait]
impl UploadPublisher for DurableNamespace {
    async fn verify_and_close(
        &self,
        id: UploadId,
        expected_size: i64,
    ) -> Result<VerifiedUpload, UploadPublicationError> {
        let staging = self.staging.lock().unwrap();
        let (identity, bytes) = staging.get(&id).ok_or(UploadPublicationError::NotFound)?;
        let size = i64::try_from(bytes.len()).map_err(|_| UploadPublicationError::Unavailable)?;
        if size != expected_size {
            return Err(UploadPublicationError::Conflict);
        }
        Ok(VerifiedUpload {
            size,
            sha256: Sha256::digest(bytes).into(),
            staging_identity: *identity,
        })
    }

    async fn observe(
        &self,
        intent: &UploadCommitIntent,
    ) -> Result<UploadPublicationObservation, UploadPublicationError> {
        let source = self.staging.lock().unwrap().get(&intent.upload_id).map_or(
            PublicationPresence::Absent,
            |(identity, _)| {
                if *identity == intent.staging_identity {
                    PublicationPresence::Expected
                } else {
                    PublicationPresence::Unexpected
                }
            },
        );
        let destination = self
            .destinations
            .lock()
            .unwrap()
            .get(&Self::key(intent))
            .map_or(PublicationPresence::Absent, |destination| {
                if destination.identity == intent.staging_identity {
                    PublicationPresence::Expected
                } else {
                    PublicationPresence::Unexpected
                }
            });
        Ok(UploadPublicationObservation {
            staging: source,
            destination,
        })
    }

    async fn publish_no_replace(
        &self,
        intent: &UploadCommitIntent,
    ) -> Result<PublishedUpload, UploadPublicationError> {
        let mut destinations = self.destinations.lock().unwrap();
        let key = Self::key(intent);
        if destinations.contains_key(&key) {
            return Err(UploadPublicationError::Conflict);
        }
        let (identity, bytes) = self
            .staging
            .lock()
            .unwrap()
            .remove(&intent.upload_id)
            .ok_or(UploadPublicationError::NotFound)?;
        let size = i64::try_from(bytes.len()).map_err(|_| UploadPublicationError::Unavailable)?;
        destinations.insert(key, Destination { identity, bytes });
        Ok(PublishedUpload {
            identity,
            size,
            mtime_filetime_100ns: 321,
        })
    }

    async fn inspect_destination(
        &self,
        intent: &UploadCommitIntent,
    ) -> Result<PublishedUpload, UploadPublicationError> {
        let destinations = self.destinations.lock().unwrap();
        let destination = destinations
            .get(&Self::key(intent))
            .ok_or(UploadPublicationError::NotFound)?;
        Ok(PublishedUpload {
            identity: destination.identity,
            size: i64::try_from(destination.bytes.len())
                .map_err(|_| UploadPublicationError::Unavailable)?,
            mtime_filetime_100ns: 321,
        })
    }
}

struct Harness {
    _directory: TempDir,
    pool: sqlx::SqlitePool,
    namespace: Arc<DurableNamespace>,
    project_id: cellar_core::ProjectId,
    now: OffsetDateTime,
}

impl Harness {
    async fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let pool = open_pool(
            directory.path().join("cellar.db"),
            FilenameCollation::windows_ordinal_ci_v1(str::cmp),
        )
        .await
        .unwrap();
        migrate(&pool).await.unwrap();
        let project_id = cellar_core::ProjectId::new();
        sqlx::query(
            "INSERT INTO project
             (id, name, description, status, version, created_at, updated_at)
             VALUES (?, 'faults', '', 'active', 1,
                     '1970-01-01T00:00:00.000000000Z',
                     '1970-01-01T00:00:00.000000000Z')",
        )
        .bind(project_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
        Self {
            _directory: directory,
            pool,
            namespace: Arc::new(DurableNamespace::default()),
            project_id,
            now: OffsetDateTime::from_unix_timestamp(50_000).unwrap(),
        }
    }

    fn service(&self) -> UploadService {
        UploadService::with_finalization(
            Arc::new(SqliteUploadRepository::new(self.pool.clone())),
            self.namespace.clone(),
            Arc::new(SqliteOperationRepository::new(self.pool.clone())),
            self.namespace.clone(),
            UploadLimits {
                max_chunk_size: 1024,
                max_active_sessions: 8,
                max_concurrent_uploads: 3,
                free_space_reserve: 0,
                session_ttl: Duration::days(7),
            },
        )
    }

    async fn uploaded(&self, name: &str) -> UploadId {
        let service = self.service();
        let session = service
            .create(
                NewUpload {
                    project_id: self.project_id,
                    destination_parent_id: None,
                    destination_name: name.to_owned(),
                    expected_size: 3,
                    expected_hash: Some(Sha256::digest(b"abc").into()),
                },
                self.now,
            )
            .await
            .unwrap();
        service
            .put_chunk(
                session.id,
                0,
                b"abc",
                Sha256::digest(b"abc").into(),
                self.now,
            )
            .await
            .unwrap();
        session.id
    }
}

#[tokio::test]
async fn process_restart_at_each_finalize_fault_point_never_duplicates_or_exposes_partial_data() {
    for point in [
        FaultPoint::Intent,
        FaultPoint::Rename,
        FaultPoint::CatalogCommit,
    ] {
        let harness = Harness::new().await;
        let id = harness.uploaded(&format!("{point:?}.bin")).await;
        let operations = SqliteOperationRepository::new(harness.pool.clone());
        let verified = harness.namespace.verify_and_close(id, 3).await.unwrap();
        let intent = match operations
            .prepare_upload_commit(id, verified, harness.now)
            .await
            .unwrap()
        {
            UploadFinalizeStart::Intent(intent) => intent,
            UploadFinalizeStart::Completed(_) => panic!("new upload unexpectedly complete"),
        };

        match point {
            FaultPoint::Intent => {}
            FaultPoint::Rename => {
                harness.namespace.publish_no_replace(&intent).await.unwrap();
            }
            FaultPoint::CatalogCommit => {
                let published = harness.namespace.publish_no_replace(&intent).await.unwrap();
                let applied = operations
                    .mark_upload_fs_applied(&intent, published, harness.now)
                    .await
                    .unwrap();
                operations
                    .complete_upload_commit(&applied, published, harness.now)
                    .await
                    .unwrap();
            }
        }

        let restarted = harness.service();
        restarted.initialize(harness.now).await.unwrap();
        let entry = restarted.finalize(id, harness.now).await.unwrap();
        assert_eq!(entry.exact_name.as_str(), format!("{point:?}.bin"));
        assert!(!harness.namespace.staging.lock().unwrap().contains_key(&id));
        assert_eq!(harness.namespace.destinations.lock().unwrap().len(), 1);
        let counts: (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM file_entry),
                    (SELECT count(*) FROM operation WHERE state = 'complete'),
                    (SELECT count(*) FROM upload_session WHERE state = 'complete'),
                    (SELECT count(*) FROM operation WHERE state IN ('pending', 'fs_applied'))",
        )
        .fetch_one(&harness.pool)
        .await
        .unwrap();
        assert_eq!(counts, (1, 1, 1, 0));
        harness.pool.close().await;
    }
}

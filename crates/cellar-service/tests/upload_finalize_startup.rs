#![cfg(windows)]

use std::sync::Arc;

use cellar_api::health::Readiness;
use cellar_core::{
    NewUpload, ReadinessBlocker, UploadFinalizeRepository, UploadFinalizeStart, UploadPublisher,
};
use cellar_db::{FilenameCollation, SqliteOperationRepository};
use cellar_service::recovery::initialize_upload_finalization_recovery;
use cellar_windows::{WindowsStorage, WindowsUploadStaging};
use sha2::{Digest as _, Sha256};
use tempfile::tempdir;
use time::OffsetDateTime;

#[tokio::test]
async fn production_restart_recovers_a_renamed_upload_before_readiness() {
    let database = tempdir().unwrap();
    let storage_root = tempdir().unwrap();
    let project_id = cellar_core::ProjectId::new();
    std::fs::create_dir_all(
        storage_root
            .path()
            .join("projects")
            .join(project_id.to_string())
            .join("files"),
    )
    .unwrap();
    let pool = cellar_db::open_pool(
        database.path().join("cellar.db"),
        FilenameCollation::windows_ordinal_ci_v1(str::cmp),
    )
    .await
    .unwrap();
    cellar_db::migrate(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO project
         (id, name, description, status, version, created_at, updated_at)
         VALUES (?, 'production', '', 'active', 1,
                 '1970-01-01T00:00:00.000000000Z',
                 '1970-01-01T00:00:00.000000000Z')",
    )
    .bind(project_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    let now = OffsetDateTime::from_unix_timestamp(50_000).unwrap();
    let adapter = open_staging(storage_root.path());
    let readiness = Readiness::new([ReadinessBlocker::RecoveryRequired]);
    let service = initialize_upload_finalization_recovery(
        &pool,
        adapter.clone(),
        adapter.clone(),
        &readiness,
        now,
    )
    .await
    .unwrap();
    let session = service
        .create(
            NewUpload {
                project_id,
                destination_parent_id: None,
                destination_name: "restart.bin".into(),
                expected_size: 3,
                expected_hash: Some(Sha256::digest(b"abc").into()),
            },
            now,
        )
        .await
        .unwrap();
    service
        .put_chunk(session.id, 0, b"abc", Sha256::digest(b"abc").into(), now)
        .await
        .unwrap();

    let operations = SqliteOperationRepository::new(pool.clone());
    let target = operations.upload_finalize_target(session.id).await.unwrap();
    let verified = adapter
        .verify_and_retain(session.id, &target, 3)
        .await
        .unwrap();
    let facts = verified.facts();
    let intent = match operations
        .prepare_upload_commit(session.id, &target, facts, now)
        .await
        .unwrap()
    {
        UploadFinalizeStart::Intent(intent) => intent,
        UploadFinalizeStart::Completed(_) => panic!("fresh upload was already complete"),
    };
    adapter.publish_no_replace(&intent, verified).await.unwrap();
    drop(service);
    drop(adapter);

    let restarted_adapter = open_staging(storage_root.path());
    let restarted_readiness = Readiness::new([ReadinessBlocker::RecoveryRequired]);
    let restarted = initialize_upload_finalization_recovery(
        &pool,
        restarted_adapter.clone(),
        restarted_adapter,
        &restarted_readiness,
        now,
    )
    .await
    .unwrap();
    assert!(restarted_readiness.is_ready());
    let entry = restarted.finalize(session.id, now).await.unwrap();
    assert_eq!(entry.exact_name.as_str(), "restart.bin");
    assert_eq!(
        std::fs::read(
            storage_root
                .path()
                .join("projects")
                .join(project_id.to_string())
                .join("files")
                .join("restart.bin"),
        )
        .unwrap(),
        b"abc"
    );
    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM file_entry),
                (SELECT count(*) FROM operation WHERE state = 'complete'),
                (SELECT count(*) FROM upload_session WHERE state = 'complete')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(counts, (1, 1, 1));
    pool.close().await;
}

#[tokio::test]
async fn substituted_root_destination_namespace_blocks_recovery_before_publish() {
    let database = tempdir().unwrap();
    let storage_root = tempdir().unwrap();
    let project_id = cellar_core::ProjectId::new();
    let project_root = storage_root
        .path()
        .join("projects")
        .join(project_id.to_string());
    let files = project_root.join("files");
    std::fs::create_dir_all(&files).unwrap();
    let pool = cellar_db::open_pool(
        database.path().join("cellar.db"),
        FilenameCollation::windows_ordinal_ci_v1(str::cmp),
    )
    .await
    .unwrap();
    cellar_db::migrate(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO project
         (id, name, description, status, version, created_at, updated_at)
         VALUES (?, 'root-substitution', '', 'active', 1,
                 '1970-01-01T00:00:00.000000000Z',
                 '1970-01-01T00:00:00.000000000Z')",
    )
    .bind(project_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    let now = OffsetDateTime::from_unix_timestamp(50_000).unwrap();
    let adapter = open_staging(storage_root.path());
    let readiness = Readiness::new([ReadinessBlocker::RecoveryRequired]);
    let service = initialize_upload_finalization_recovery(
        &pool,
        adapter.clone(),
        adapter.clone(),
        &readiness,
        now,
    )
    .await
    .unwrap();
    let session = service
        .create(
            NewUpload {
                project_id,
                destination_parent_id: None,
                destination_name: "must-not-redirect.bin".into(),
                expected_size: 3,
                expected_hash: Some(Sha256::digest(b"abc").into()),
            },
            now,
        )
        .await
        .unwrap();
    service
        .put_chunk(session.id, 0, b"abc", Sha256::digest(b"abc").into(), now)
        .await
        .unwrap();
    let operations = SqliteOperationRepository::new(pool.clone());
    let target = operations.upload_finalize_target(session.id).await.unwrap();
    let verified = adapter
        .verify_and_retain(session.id, &target, 3)
        .await
        .unwrap();
    let facts = verified.facts();
    let intent = match operations
        .prepare_upload_commit(session.id, &target, facts, now)
        .await
        .unwrap()
    {
        UploadFinalizeStart::Intent(intent) => intent,
        UploadFinalizeStart::Completed(_) => panic!("fresh upload was already complete"),
    };
    assert_eq!(
        intent.destination_namespace_identity,
        facts.destination_namespace_identity
    );
    let mapped_namespace: Vec<u8> = sqlx::query_scalar(
        "SELECT destination_namespace_identity FROM upload_finalization WHERE upload_id = ?",
    )
    .bind(session.id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        mapped_namespace,
        facts.destination_namespace_identity.as_bytes()
    );
    drop(verified);
    drop(service);
    drop(adapter);

    let original_files = project_root.join("files.original");
    std::fs::rename(&files, &original_files).unwrap();
    std::fs::create_dir(&files).unwrap();
    let restarted_adapter = open_staging(storage_root.path());
    let restarted_readiness = Readiness::new([ReadinessBlocker::RecoveryRequired]);
    assert!(
        initialize_upload_finalization_recovery(
            &pool,
            restarted_adapter.clone(),
            restarted_adapter,
            &restarted_readiness,
            now,
        )
        .await
        .is_err()
    );
    assert!(!restarted_readiness.is_ready());
    assert!(!files.join("must-not-redirect.bin").exists());
    assert!(!original_files.join("must-not-redirect.bin").exists());
    assert!(
        storage_root
            .path()
            .join(".cellar-upload-staging")
            .join(format!("{}.part", session.id))
            .exists()
    );
    pool.close().await;
}

fn open_staging(root: &std::path::Path) -> Arc<WindowsUploadStaging> {
    let identity = cellar_windows::preflight::open_as_service(root).unwrap();
    let storage = WindowsStorage::adopt(identity).unwrap();
    Arc::new(WindowsUploadStaging::open(storage).unwrap())
}

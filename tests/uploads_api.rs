mod common;

use std::{
    io::{self, Write},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use axum::body::Body;
use cellar::{
    db::{Database, DbError, NewUpload, ProjectRow, UploadRow},
    storage::{SafeFileName, Storage, StorageError},
    uploads::{
        DecimalU64, UploadRepository, UploadRepositoryFuture, UploadService, UploadStorage,
        UploadStorageFuture,
    },
};
use serde_json::json;
use tokio::sync::{Barrier, Semaphore, oneshot};
use tower::ServiceExt;
use uuid::{Uuid, Version};

use common::{
    OWNER_EMAIL, TestContext, VALID_ASSERTION, authenticated_request, authenticated_write_request,
    json_request, json_response,
};

async fn create_project(context: &TestContext) -> Uuid {
    let response = context
        .app()
        .oneshot(json_request(
            authenticated_write_request("/api/v1/projects"),
            r#"{"name":"Uploads"}"#,
        ))
        .await
        .unwrap();
    let (status, _, body) = json_response(response).await;
    assert_eq!(status, 201);
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

fn uploads(project_id: Uuid) -> String {
    format!("/api/v1/projects/{project_id}/uploads")
}

fn upload(upload_id: Uuid) -> String {
    format!("/api/v1/uploads/{upload_id}")
}

fn upload_chunk(upload_id: Uuid) -> String {
    format!("/api/v1/uploads/{upload_id}/chunk")
}

fn upload_complete(upload_id: Uuid) -> String {
    format!("/api/v1/uploads/{upload_id}/complete")
}

async fn create_upload_session(context: &TestContext, project_id: Uuid, total_size: u64) -> Uuid {
    let response = context
        .app()
        .oneshot(json_request(
            authenticated_write_request(&uploads(project_id)),
            &format!(r#"{{"fileName":"chunk.bin","totalSize":"{total_size}"}}"#),
        ))
        .await
        .unwrap();
    let (status, _, body) = json_response(response).await;
    assert_eq!(status, 201);
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn upload_completion_publishes_exact_bytes_and_is_idempotent() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 4).await;
    assert_eq!(
        context
            .app()
            .oneshot(chunk_request(upload_id, "0", 4, Body::from("data")))
            .await
            .unwrap()
            .status(),
        204
    );

    for _ in 0..2 {
        let response = context
            .app()
            .oneshot(
                authenticated_write_request(&upload_complete(upload_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, _, body) = json_response(response).await;
        assert_eq!(status, 200);
        assert_eq!(body["id"], upload_id.to_string());
        assert_eq!(body["projectId"], project_id.to_string());
        assert_eq!(body["fileName"], "chunk.bin");
        assert_eq!(body["totalSize"], "4");
        assert_eq!(body["committedOffset"], "4");
        assert_eq!(body["state"], "complete");
    }

    assert_eq!(
        std::fs::read(context.project_path(project_id).join("files/chunk.bin")).unwrap(),
        b"data"
    );
    assert_eq!(context.storage.staging_len(upload_id).await.unwrap(), None);
    assert_eq!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .unwrap()
            .state(),
        cellar::db::UploadState::Complete
    );
    context.close().await;
}

#[tokio::test]
async fn upload_completion_rejects_incomplete_session_without_changes() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 4).await;

    let response = context
        .app()
        .oneshot(
            authenticated_write_request(&upload_complete(upload_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _, body) = json_response(response).await;
    assert_eq!(status, 409);
    assert_eq!(body["error"]["code"], "upload_incomplete");
    let row = context
        .database
        .get_upload(upload_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state(), cellar::db::UploadState::Active);
    assert_eq!(row.committed_offset(), 0);
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0)
    );
    assert!(
        !context
            .project_path(project_id)
            .join("files/chunk.bin")
            .exists()
    );
    context.close().await;
}

#[tokio::test]
async fn upload_complete_rejects_missing_short_and_long_staging_before_finalizing() {
    for (case, mutate) in [("missing", 0_u8), ("short", 1_u8), ("long", 2_u8)] {
        let context = TestContext::new().await;
        let project_id = create_project(&context).await;
        let upload_id = create_upload_session(&context, project_id, 4).await;
        assert_eq!(
            context
                .app()
                .oneshot(chunk_request(upload_id, "0", 4, Body::from("data")))
                .await
                .unwrap()
                .status(),
            204
        );
        match mutate {
            0 => context.storage.remove_staging(upload_id).await.unwrap(),
            1 => context
                .storage
                .truncate_staging(upload_id, 3)
                .await
                .unwrap(),
            2 => {
                context
                    .storage
                    .write_chunk(upload_id, 4, &b"x"[..])
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }

        let response = context
            .app()
            .oneshot(
                authenticated_write_request(&upload_complete(upload_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, _, body) = json_response(response).await;
        assert_eq!(status, 409, "unexpected status for {case}");
        assert_eq!(body["error"]["code"], "upload_staging_invalid");
        assert_eq!(
            context
                .database
                .get_upload(upload_id)
                .await
                .unwrap()
                .unwrap()
                .state(),
            cellar::db::UploadState::Failed
        );
        assert!(
            !context
                .project_path(project_id)
                .join("files/chunk.bin")
                .exists()
        );
        context.close().await;
    }
}

#[tokio::test]
async fn upload_complete_accepts_exact_zero_byte_staging() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 0).await;

    let response = context
        .app()
        .oneshot(
            authenticated_write_request(&upload_complete(upload_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _, body) = json_response(response).await;
    assert_eq!(status, 200);
    assert_eq!(body["state"], "complete");
    assert_eq!(
        std::fs::metadata(context.project_path(project_id).join("files/chunk.bin"))
            .unwrap()
            .len(),
        0
    );
    context.close().await;
}

#[tokio::test]
async fn upload_complete_preserves_unsafe_exact_entries_and_marks_failed_before_finalizing() {
    for unsafe_destination in [false, true] {
        let context = TestContext::new().await;
        let project_id = create_project(&context).await;
        let upload_id = create_upload_session(&context, project_id, 0).await;
        let unsafe_path = if unsafe_destination {
            context.project_path(project_id).join("files/chunk.bin")
        } else {
            let path = context
                .temp
                .path()
                .join(".cellar/uploads")
                .join(format!("{upload_id}.part"));
            context.storage.remove_staging(upload_id).await.unwrap();
            path
        };
        std::fs::create_dir(&unsafe_path).unwrap();

        let response = context
            .app()
            .oneshot(
                authenticated_write_request(&upload_complete(upload_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, _, body) = json_response(response).await;
        assert_eq!(status, 409);
        assert_eq!(
            body["error"]["code"],
            if unsafe_destination {
                "destination_exists"
            } else {
                "upload_staging_invalid"
            }
        );
        assert_eq!(
            context
                .database
                .get_upload(upload_id)
                .await
                .unwrap()
                .unwrap()
                .state(),
            cellar::db::UploadState::Failed
        );
        assert!(std::fs::metadata(unsafe_path).unwrap().is_dir());
        context.close().await;
    }
}

struct PreflightOrderingRepository {
    database: Database,
    staging_checked: Arc<AtomicBool>,
    destination_checked: Arc<AtomicBool>,
}

impl UploadRepository for PreflightOrderingRepository {
    fn get_project<'a>(
        &'a self,
        project_id: Uuid,
    ) -> UploadRepositoryFuture<'a, Option<ProjectRow>> {
        Box::pin(async move { self.database.get_project(project_id).await })
    }

    fn create_upload<'a>(&'a self, upload: NewUpload) -> UploadRepositoryFuture<'a, UploadRow> {
        Box::pin(async move { self.database.create_upload(upload).await })
    }

    fn get_upload<'a>(&'a self, upload_id: Uuid) -> UploadRepositoryFuture<'a, Option<UploadRow>> {
        Box::pin(async move { self.database.get_upload(upload_id).await })
    }

    fn mark_upload_finalizing<'a>(
        &'a self,
        upload_id: Uuid,
        expected: u64,
    ) -> UploadRepositoryFuture<'a, bool> {
        Box::pin(async move {
            assert!(self.staging_checked.load(Ordering::SeqCst));
            assert!(self.destination_checked.load(Ordering::SeqCst));
            self.database.mark_finalizing(upload_id, expected).await
        })
    }

    fn mark_upload_complete<'a>(&'a self, upload_id: Uuid) -> UploadRepositoryFuture<'a, ()> {
        Box::pin(async move { self.database.mark_complete(upload_id).await })
    }
}

struct PreflightOrderingStorage {
    storage: Arc<Storage>,
    staging_checked: Arc<AtomicBool>,
    destination_checked: Arc<AtomicBool>,
}

impl UploadStorage for PreflightOrderingStorage {
    fn destination_exists<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, bool> {
        Box::pin(async move { self.storage.destination_exists(project_id, file_name).await })
    }

    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.create_staging(upload_id).await })
    }

    fn remove_empty_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.remove_empty_staging(upload_id).await })
    }

    fn staging_len<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, Option<u64>> {
        Box::pin(async move {
            let result = self.storage.staging_len(upload_id).await;
            self.staging_checked.store(true, Ordering::SeqCst);
            result
        })
    }

    fn final_file_len<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, Option<u64>> {
        Box::pin(async move {
            let result = self.storage.final_file_len(project_id, file_name).await;
            self.destination_checked.store(true, Ordering::SeqCst);
            result
        })
    }

    fn sync_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.sync_staging(upload_id).await })
    }

    fn finalize_no_replace<'a>(
        &'a self,
        upload_id: Uuid,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move {
            self.storage
                .finalize_no_replace(upload_id, project_id, file_name)
                .await
        })
    }
}

#[tokio::test]
async fn upload_complete_inspects_exact_staging_and_destination_before_mark_finalizing() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 0).await;
    let staging_checked = Arc::new(AtomicBool::new(false));
    let destination_checked = Arc::new(AtomicBool::new(false));
    let service = UploadService::new(
        Arc::new(PreflightOrderingRepository {
            database: context.database.clone(),
            staging_checked: staging_checked.clone(),
            destination_checked: destination_checked.clone(),
        }),
        Arc::new(PreflightOrderingStorage {
            storage: context.storage.clone(),
            staging_checked,
            destination_checked,
        }),
    );

    let response = context
        .secure_upload(service)
        .oneshot(
            authenticated_write_request(&upload_complete(upload_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    context.close().await;
}

#[tokio::test]
async fn upload_completion_conflict_preserves_destination_and_marks_failed() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 4).await;
    assert_eq!(
        context
            .app()
            .oneshot(chunk_request(upload_id, "0", 4, Body::from("data")))
            .await
            .unwrap()
            .status(),
        204
    );
    let destination = context.project_path(project_id).join("files/chunk.bin");
    std::fs::write(&destination, b"keep").unwrap();

    let response = context
        .app()
        .oneshot(
            authenticated_write_request(&upload_complete(upload_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _, body) = json_response(response).await;
    assert_eq!(status, 409);
    assert_eq!(body["error"]["code"], "destination_exists");
    assert_eq!(std::fs::read(destination).unwrap(), b"keep");
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(4)
    );
    assert_eq!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .unwrap()
            .state(),
        cellar::db::UploadState::Failed
    );
    context.close().await;
}

struct FinalizeObservingStorage {
    storage: Arc<Storage>,
    database: Database,
    move_started: Mutex<Option<oneshot::Sender<()>>>,
    release_move: Option<Arc<Semaphore>>,
}

impl UploadStorage for FinalizeObservingStorage {
    fn destination_exists<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, bool> {
        Box::pin(async move { self.storage.destination_exists(project_id, file_name).await })
    }

    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.create_staging(upload_id).await })
    }

    fn remove_empty_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.remove_empty_staging(upload_id).await })
    }

    fn staging_len<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, Option<u64>> {
        Box::pin(async move { self.storage.staging_len(upload_id).await })
    }

    fn sync_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.sync_staging(upload_id).await })
    }

    fn finalize_no_replace<'a>(
        &'a self,
        upload_id: Uuid,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, ()> {
        let started = self.move_started.lock().unwrap().take();
        Box::pin(async move {
            assert_eq!(
                self.database
                    .get_upload(upload_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state(),
                cellar::db::UploadState::Finalizing
            );
            if let Some(started) = started {
                let _ = started.send(());
            }
            if let Some(release) = &self.release_move {
                release.acquire().await.unwrap().forget();
            }
            self.storage
                .finalize_no_replace(upload_id, project_id, file_name)
                .await
        })
    }

    fn final_file_len<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, Option<u64>> {
        Box::pin(async move { self.storage.final_file_len(project_id, file_name).await })
    }
}

#[tokio::test]
async fn upload_completion_marks_finalizing_before_move() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 4).await;
    assert_eq!(
        context
            .app()
            .oneshot(chunk_request(upload_id, "0", 4, Body::from("data")))
            .await
            .unwrap()
            .status(),
        204
    );
    let service = UploadService::new(
        Arc::new(context.database.clone()),
        Arc::new(FinalizeObservingStorage {
            storage: context.storage.clone(),
            database: context.database.clone(),
            move_started: Mutex::new(None),
            release_move: None,
        }),
    );
    let response = context
        .secure_upload(service)
        .oneshot(
            authenticated_write_request(&upload_complete(upload_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    context.close().await;
}

#[tokio::test]
async fn upload_completion_request_abort_after_finalizing_finishes_detached_operation() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 4).await;
    assert_eq!(
        context
            .app()
            .oneshot(chunk_request(upload_id, "0", 4, Body::from("data")))
            .await
            .unwrap()
            .status(),
        204
    );
    let (started_tx, started_rx) = oneshot::channel();
    let release = Arc::new(Semaphore::new(0));
    let service = UploadService::new(
        Arc::new(context.database.clone()),
        Arc::new(FinalizeObservingStorage {
            storage: context.storage.clone(),
            database: context.database.clone(),
            move_started: Mutex::new(Some(started_tx)),
            release_move: Some(release.clone()),
        }),
    );
    let request = tokio::spawn(
        context.secure_upload(service).oneshot(
            authenticated_write_request(&upload_complete(upload_id))
                .body(Body::empty())
                .unwrap(),
        ),
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .unwrap()
            .state(),
        cellar::db::UploadState::Finalizing
    );
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    release.add_permits(1);

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if context
                .database
                .get_upload(upload_id)
                .await
                .unwrap()
                .unwrap()
                .state()
                == cellar::db::UploadState::Complete
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached finalization did not complete");
    assert_eq!(
        std::fs::read(context.project_path(project_id).join("files/chunk.bin")).unwrap(),
        b"data"
    );
    context.close().await;
}

#[derive(Clone, Copy)]
enum FinalizeFailureMode {
    SyncFull,
    SyncUnsafe,
    MoveUnavailable,
    MoveUnsafe,
    VerifyMismatch,
    VerifyUnsafe,
    PanicAfterMove,
}

struct FailingFinalizeStorage {
    storage: Arc<Storage>,
    mode: FinalizeFailureMode,
    final_len_calls: AtomicUsize,
}

impl UploadStorage for FailingFinalizeStorage {
    fn destination_exists<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, bool> {
        Box::pin(async move { self.storage.destination_exists(project_id, file_name).await })
    }

    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.create_staging(upload_id).await })
    }

    fn remove_empty_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.remove_empty_staging(upload_id).await })
    }

    fn staging_len<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, Option<u64>> {
        Box::pin(async move { self.storage.staging_len(upload_id).await })
    }

    fn sync_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move {
            if matches!(self.mode, FinalizeFailureMode::SyncFull) {
                Err(StorageError::InsufficientSpace)
            } else if matches!(self.mode, FinalizeFailureMode::SyncUnsafe) {
                Err(StorageError::UnsafeEntry)
            } else {
                self.storage.sync_staging(upload_id).await
            }
        })
    }

    fn finalize_no_replace<'a>(
        &'a self,
        upload_id: Uuid,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move {
            if matches!(self.mode, FinalizeFailureMode::MoveUnavailable) {
                Err(StorageError::Io {
                    source: io::Error::other("private move failure"),
                })
            } else if matches!(self.mode, FinalizeFailureMode::MoveUnsafe) {
                Err(StorageError::UnsafeEntry)
            } else if matches!(self.mode, FinalizeFailureMode::PanicAfterMove) {
                self.storage
                    .finalize_no_replace(upload_id, project_id, file_name)
                    .await?;
                panic!("deterministic panic after final move")
            } else {
                self.storage
                    .finalize_no_replace(upload_id, project_id, file_name)
                    .await
            }
        })
    }

    fn final_file_len<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, Option<u64>> {
        Box::pin(async move {
            let call = self.final_len_calls.fetch_add(1, Ordering::SeqCst);
            if matches!(self.mode, FinalizeFailureMode::VerifyMismatch) && call > 0 {
                Ok(Some(99))
            } else if matches!(self.mode, FinalizeFailureMode::VerifyUnsafe) && call > 0 {
                Err(StorageError::UnsafeEntry)
            } else {
                self.storage.final_file_len(project_id, file_name).await
            }
        })
    }
}

#[tokio::test]
async fn upload_completion_storage_failures_leave_recoverable_evidence() {
    for (mode, expected_status, staging_exists, destination_exists) in [
        (FinalizeFailureMode::SyncFull, 507, true, false),
        (FinalizeFailureMode::MoveUnavailable, 503, true, false),
        (FinalizeFailureMode::VerifyMismatch, 503, false, true),
    ] {
        let context = TestContext::new().await;
        let project_id = create_project(&context).await;
        let upload_id = create_upload_session(&context, project_id, 4).await;
        assert_eq!(
            context
                .app()
                .oneshot(chunk_request(upload_id, "0", 4, Body::from("data")))
                .await
                .unwrap()
                .status(),
            204
        );
        let service = UploadService::new(
            Arc::new(context.database.clone()),
            Arc::new(FailingFinalizeStorage {
                storage: context.storage.clone(),
                mode,
                final_len_calls: AtomicUsize::new(0),
            }),
        );
        let response = context
            .secure_upload(service)
            .oneshot(
                authenticated_write_request(&upload_complete(upload_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected_status);
        assert_eq!(
            context
                .database
                .get_upload(upload_id)
                .await
                .unwrap()
                .unwrap()
                .state(),
            cellar::db::UploadState::Finalizing
        );
        assert_eq!(
            context
                .storage
                .staging_len(upload_id)
                .await
                .unwrap()
                .is_some(),
            staging_exists
        );
        assert_eq!(
            context
                .project_path(project_id)
                .join("files/chunk.bin")
                .exists(),
            destination_exists
        );
        context.close().await;
    }
}

#[tokio::test]
async fn upload_complete_late_unsafe_entries_mark_failed_and_return_409() {
    for (mode, staging_exists, destination_exists, expected_code) in [
        (
            FinalizeFailureMode::SyncUnsafe,
            true,
            false,
            "upload_staging_invalid",
        ),
        (
            FinalizeFailureMode::MoveUnsafe,
            true,
            false,
            "destination_exists",
        ),
        (
            FinalizeFailureMode::VerifyUnsafe,
            false,
            true,
            "destination_exists",
        ),
    ] {
        let context = TestContext::new().await;
        let project_id = create_project(&context).await;
        let upload_id = create_upload_session(&context, project_id, 0).await;
        let response = context
            .secure_upload(UploadService::new(
                Arc::new(context.database.clone()),
                Arc::new(FailingFinalizeStorage {
                    storage: context.storage.clone(),
                    mode,
                    final_len_calls: AtomicUsize::new(0),
                }),
            ))
            .oneshot(
                authenticated_write_request(&upload_complete(upload_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, _, body) = json_response(response).await;

        assert_eq!(status, 409);
        assert_eq!(body["error"]["code"], expected_code);
        assert_eq!(
            context
                .database
                .get_upload(upload_id)
                .await
                .unwrap()
                .unwrap()
                .state(),
            cellar::db::UploadState::Failed
        );
        assert_eq!(
            context
                .storage
                .staging_len(upload_id)
                .await
                .unwrap()
                .is_some(),
            staging_exists
        );
        assert_eq!(
            context
                .project_path(project_id)
                .join("files/chunk.bin")
                .exists(),
            destination_exists
        );
        context.close().await;
    }
}

#[derive(Clone, Copy)]
enum PreflightDiskFullMode {
    Staging,
    Destination,
}

struct PreflightDiskFullStorage {
    storage: Arc<Storage>,
    mode: PreflightDiskFullMode,
}

impl UploadStorage for PreflightDiskFullStorage {
    fn destination_exists<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, bool> {
        Box::pin(async move { self.storage.destination_exists(project_id, file_name).await })
    }

    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.create_staging(upload_id).await })
    }

    fn remove_empty_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.remove_empty_staging(upload_id).await })
    }

    fn staging_len<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, Option<u64>> {
        Box::pin(async move {
            if matches!(self.mode, PreflightDiskFullMode::Staging) {
                Err(StorageError::InsufficientSpace)
            } else {
                self.storage.staging_len(upload_id).await
            }
        })
    }

    fn final_file_len<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, Option<u64>> {
        Box::pin(async move {
            if matches!(self.mode, PreflightDiskFullMode::Destination) {
                Err(StorageError::InsufficientSpace)
            } else {
                self.storage.final_file_len(project_id, file_name).await
            }
        })
    }
}

#[tokio::test]
async fn upload_complete_preflight_disk_full_is_507_without_transition_or_mutation() {
    for mode in [
        PreflightDiskFullMode::Staging,
        PreflightDiskFullMode::Destination,
    ] {
        let context = TestContext::new().await;
        let project_id = create_project(&context).await;
        let upload_id = create_upload_session(&context, project_id, 0).await;
        let response = context
            .secure_upload(UploadService::new(
                Arc::new(context.database.clone()),
                Arc::new(PreflightDiskFullStorage {
                    storage: context.storage.clone(),
                    mode,
                }),
            ))
            .oneshot(
                authenticated_write_request(&upload_complete(upload_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 507);
        assert_eq!(
            context
                .database
                .get_upload(upload_id)
                .await
                .unwrap()
                .unwrap()
                .state(),
            cellar::db::UploadState::Active
        );
        assert_eq!(
            context.storage.staging_len(upload_id).await.unwrap(),
            Some(0)
        );
        assert!(
            !context
                .project_path(project_id)
                .join("files/chunk.bin")
                .exists()
        );
        context.close().await;
    }
}

struct CompleteFailingRepository {
    database: Database,
}

struct CompletionFailureMarkFailingRepository {
    database: Database,
}

impl UploadRepository for CompletionFailureMarkFailingRepository {
    fn get_project<'a>(
        &'a self,
        project_id: Uuid,
    ) -> UploadRepositoryFuture<'a, Option<ProjectRow>> {
        Box::pin(async move { self.database.get_project(project_id).await })
    }

    fn create_upload<'a>(&'a self, upload: NewUpload) -> UploadRepositoryFuture<'a, UploadRow> {
        Box::pin(async move { self.database.create_upload(upload).await })
    }

    fn get_upload<'a>(&'a self, upload_id: Uuid) -> UploadRepositoryFuture<'a, Option<UploadRow>> {
        Box::pin(async move { self.database.get_upload(upload_id).await })
    }

    fn mark_upload_failed<'a>(&'a self, _: Uuid, _: &'a str) -> UploadRepositoryFuture<'a, ()> {
        Box::pin(async { Err(DbError::InvalidTransition) })
    }

    fn mark_upload_finalizing<'a>(
        &'a self,
        upload_id: Uuid,
        expected: u64,
    ) -> UploadRepositoryFuture<'a, bool> {
        Box::pin(async move { self.database.mark_finalizing(upload_id, expected).await })
    }
}

#[tokio::test]
async fn upload_complete_preflight_failure_transition_error_returns_503_without_false_state() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 0).await;
    let destination = context.project_path(project_id).join("files/chunk.bin");
    std::fs::create_dir(&destination).unwrap();
    let service = UploadService::new(
        Arc::new(CompletionFailureMarkFailingRepository {
            database: context.database.clone(),
        }),
        context.storage.clone(),
    );

    let response = context
        .secure_upload(service)
        .oneshot(
            authenticated_write_request(&upload_complete(upload_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 503);
    assert_eq!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .unwrap()
            .state(),
        cellar::db::UploadState::Active
    );
    assert!(std::fs::metadata(destination).unwrap().is_dir());
    context.close().await;
}

#[tokio::test]
async fn upload_complete_managed_directory_inspection_error_returns_503_without_state_claim() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 0).await;
    context.storage.remove_staging(upload_id).await.unwrap();
    let uploads_dir = context.temp.path().join(".cellar/uploads");
    std::fs::remove_dir(&uploads_dir).unwrap();
    std::fs::write(&uploads_dir, b"preserve managed corruption").unwrap();

    let response = context
        .app()
        .oneshot(
            authenticated_write_request(&upload_complete(upload_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 503);
    assert_eq!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .unwrap()
            .state(),
        cellar::db::UploadState::Active
    );
    assert_eq!(
        std::fs::read(uploads_dir).unwrap(),
        b"preserve managed corruption"
    );
    assert!(
        !context
            .project_path(project_id)
            .join("files/chunk.bin")
            .exists()
    );
    context.close().await;
}

#[tokio::test]
async fn upload_complete_late_unsafe_failure_transition_error_returns_503_without_false_claim() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 0).await;
    let response = context
        .secure_upload(UploadService::new(
            Arc::new(CompletionFailureMarkFailingRepository {
                database: context.database.clone(),
            }),
            Arc::new(FailingFinalizeStorage {
                storage: context.storage.clone(),
                mode: FinalizeFailureMode::SyncUnsafe,
                final_len_calls: AtomicUsize::new(0),
            }),
        ))
        .oneshot(
            authenticated_write_request(&upload_complete(upload_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 503);
    assert_eq!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .unwrap()
            .state(),
        cellar::db::UploadState::Finalizing
    );
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0)
    );
    assert!(
        !context
            .project_path(project_id)
            .join("files/chunk.bin")
            .exists()
    );
    context.close().await;
}

impl UploadRepository for CompleteFailingRepository {
    fn get_project<'a>(
        &'a self,
        project_id: Uuid,
    ) -> UploadRepositoryFuture<'a, Option<ProjectRow>> {
        Box::pin(async move { self.database.get_project(project_id).await })
    }

    fn create_upload<'a>(&'a self, upload: NewUpload) -> UploadRepositoryFuture<'a, UploadRow> {
        Box::pin(async move { self.database.create_upload(upload).await })
    }

    fn get_upload<'a>(&'a self, upload_id: Uuid) -> UploadRepositoryFuture<'a, Option<UploadRow>> {
        Box::pin(async move { self.database.get_upload(upload_id).await })
    }

    fn mark_upload_finalizing<'a>(
        &'a self,
        upload_id: Uuid,
        expected: u64,
    ) -> UploadRepositoryFuture<'a, bool> {
        Box::pin(async move { self.database.mark_finalizing(upload_id, expected).await })
    }

    fn mark_upload_complete<'a>(&'a self, _: Uuid) -> UploadRepositoryFuture<'a, ()> {
        Box::pin(async { Err(DbError::InvalidTransition) })
    }
}

#[tokio::test]
async fn upload_completion_database_complete_failure_keeps_published_finalizing_evidence() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 4).await;
    assert_eq!(
        context
            .app()
            .oneshot(chunk_request(upload_id, "0", 4, Body::from("data")))
            .await
            .unwrap()
            .status(),
        204
    );
    let service = UploadService::new(
        Arc::new(CompleteFailingRepository {
            database: context.database.clone(),
        }),
        context.storage.clone(),
    );
    let response = context
        .secure_upload(service)
        .oneshot(
            authenticated_write_request(&upload_complete(upload_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .unwrap()
            .state(),
        cellar::db::UploadState::Finalizing
    );
    assert_eq!(context.storage.staging_len(upload_id).await.unwrap(), None);
    assert_eq!(
        std::fs::read(context.project_path(project_id).join("files/chunk.bin")).unwrap(),
        b"data"
    );
    context.close().await;
}

#[tokio::test]
async fn upload_complete_panic_after_move_leaves_evidence_for_focused_recovery() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 4).await;
    assert_eq!(
        context
            .app()
            .oneshot(chunk_request(upload_id, "0", 4, Body::from("data")))
            .await
            .unwrap()
            .status(),
        204
    );
    let response = context
        .secure_upload(UploadService::new(
            Arc::new(context.database.clone()),
            Arc::new(FailingFinalizeStorage {
                storage: context.storage.clone(),
                mode: FinalizeFailureMode::PanicAfterMove,
                final_len_calls: AtomicUsize::new(0),
            }),
        ))
        .oneshot(
            authenticated_write_request(&upload_complete(upload_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .unwrap()
            .state(),
        cellar::db::UploadState::Finalizing
    );
    assert_eq!(context.storage.staging_len(upload_id).await.unwrap(), None);
    assert_eq!(
        std::fs::read(context.project_path(project_id).join("files/chunk.bin")).unwrap(),
        b"data"
    );

    UploadService::new(Arc::new(context.database.clone()), context.storage.clone())
        .recover_uploads()
        .await
        .unwrap();
    assert_eq!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .unwrap()
            .state(),
        cellar::db::UploadState::Complete
    );
    context.close().await;
}

#[tokio::test]
async fn upload_completion_requires_canonical_path_authentication_and_exact_origin() {
    let context = TestContext::new().await;
    let unknown = Uuid::now_v7();
    let noncanonical = unknown.simple().to_string();
    let canonical = upload_complete(unknown);

    for (request, expected) in [
        (
            authenticated_write_request(&format!("/api/v1/uploads/{noncanonical}/complete"))
                .body(Body::empty())
                .unwrap(),
            400,
        ),
        (
            http::Request::builder()
                .method("POST")
                .uri(&canonical)
                .header("origin", common::EXTERNAL_ORIGIN)
                .body(Body::empty())
                .unwrap(),
            401,
        ),
        (
            authenticated_request("POST", &canonical)
                .header("origin", "https://evil.example")
                .body(Body::empty())
                .unwrap(),
            403,
        ),
    ] {
        let status = context.app().oneshot(request).await.unwrap().status();
        assert_eq!(status, expected);
    }
    let response = context
        .app()
        .oneshot(
            authenticated_write_request(&canonical)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    context.close().await;
}

fn chunk_request(
    upload_id: Uuid,
    offset: &str,
    declared_len: u64,
    body: Body,
) -> http::Request<Body> {
    authenticated_request("PUT", &upload_chunk(upload_id))
        .header("origin", common::EXTERNAL_ORIGIN)
        .header("content-type", "application/octet-stream")
        .header("upload-offset", offset)
        .header("content-length", declared_len)
        .body(body)
        .unwrap()
}

#[tokio::test]
async fn upload_chunk_at_exact_offset_streams_and_advances_durable_offset() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 10).await;

    let response = context
        .app()
        .oneshot(chunk_request(upload_id, "0", 4, Body::from("data")))
        .await
        .unwrap();

    assert_eq!(response.status(), 204);
    assert_eq!(response.headers()["upload-offset"], "4");
    assert_eq!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .unwrap()
            .committed_offset(),
        4
    );
    assert_eq!(
        std::fs::read(
            context
                .temp
                .path()
                .join(".cellar")
                .join("uploads")
                .join(format!("{upload_id}.part"))
        )
        .unwrap(),
        b"data"
    );
    context.close().await;
}

#[tokio::test]
async fn upload_chunk_accepts_exact_maximum_and_classifies_larger_after_session_lookup() {
    const MAX: usize = 32 * 1024 * 1024;
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, MAX as u64).await;
    let response = context
        .app()
        .oneshot(chunk_request(
            upload_id,
            "0",
            MAX as u64,
            Body::from(vec![0x5a; MAX]),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert_eq!(response.headers()["upload-offset"], MAX.to_string());
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(MAX as u64)
    );

    let oversized_upload = create_upload_session(&context, project_id, MAX as u64 + 1).await;
    let oversized = context
        .app()
        .oneshot(chunk_request(
            oversized_upload,
            "0",
            MAX as u64 + 1,
            Body::from_stream(futures_util::stream::once(async {
                Err::<bytes::Bytes, _>(io::Error::other("oversized body must not be read"))
            })),
        ))
        .await
        .unwrap();
    assert_eq!(oversized.status(), 413);
    assert_eq!(oversized.headers()["upload-offset"], "0");
    let (_, _, body) = json_response(oversized).await;
    assert_eq!(body["error"]["code"], "payload_too_large");
    assert_eq!(
        context.storage.staging_len(oversized_upload).await.unwrap(),
        Some(0)
    );

    let missing = context
        .app()
        .oneshot(chunk_request(
            Uuid::now_v7(),
            "0",
            MAX as u64 + 1,
            Body::from_stream(futures_util::stream::once(async {
                Err::<bytes::Bytes, _>(io::Error::other("unknown body must not be read"))
            })),
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    context.close().await;
}

#[tokio::test]
async fn upload_chunk_earlier_retry_must_not_cross_committed_offset() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 8).await;
    assert_eq!(
        context
            .app()
            .oneshot(chunk_request(upload_id, "0", 4, Body::from("data")))
            .await
            .unwrap()
            .status(),
        204
    );

    let boundary = context
        .app()
        .oneshot(chunk_request(upload_id, "1", 3, Body::from("old")))
        .await
        .unwrap();
    assert_eq!(boundary.status(), 204);
    assert_eq!(boundary.headers()["upload-offset"], "4");

    let crossing = context
        .app()
        .oneshot(chunk_request(
            upload_id,
            "1",
            4,
            Body::from_stream(futures_util::stream::once(async {
                Err::<bytes::Bytes, _>(io::Error::other("crossing retry body must not be read"))
            })),
        ))
        .await
        .unwrap();
    assert_eq!(crossing.status(), 409);
    assert_eq!(crossing.headers()["upload-offset"], "4");
    let (_, _, body) = json_response(crossing).await;
    assert_eq!(body["error"]["details"]["expectedOffset"], "4");
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(4)
    );

    let huge_id = create_upload_session(&context, project_id, i64::MAX as u64).await;
    assert!(
        context
            .database
            .advance_offset(huge_id, 0, i64::MAX as u64)
            .await
            .unwrap()
    );
    let last_offset = i64::MAX as u64 - 1;
    let upper_boundary = context
        .app()
        .oneshot(chunk_request(
            huge_id,
            &last_offset.to_string(),
            1,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(upper_boundary.status(), 204);
    assert_eq!(
        upper_boundary.headers()["upload-offset"],
        i64::MAX.to_string()
    );
    let upper_crossing = context
        .app()
        .oneshot(chunk_request(
            huge_id,
            &last_offset.to_string(),
            2,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(upper_crossing.status(), 409);
    assert_eq!(
        upper_crossing.headers()["upload-offset"],
        i64::MAX.to_string()
    );
    context.close().await;
}

#[tokio::test]
async fn upload_chunk_short_or_long_body_rolls_back_without_advancing() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 20).await;

    for (declared, body) in [(5, "four"), (3, "four")] {
        let response = context
            .app()
            .oneshot(chunk_request(upload_id, "0", declared, Body::from(body)))
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert_eq!(response.headers()["upload-offset"], "0");
        assert_eq!(
            context.storage.staging_len(upload_id).await.unwrap(),
            Some(0)
        );
        assert_eq!(
            context
                .database
                .get_upload(upload_id)
                .await
                .unwrap()
                .unwrap()
                .committed_offset(),
            0
        );
    }
    context.close().await;
}

#[tokio::test]
async fn upload_chunk_offset_retry_and_conflicts_are_authoritative_without_writes() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 12).await;
    let first = context
        .app()
        .oneshot(chunk_request(upload_id, "0", 4, Body::from("data")))
        .await
        .unwrap();
    assert_eq!(first.status(), 204);

    let retry = context
        .app()
        .oneshot(chunk_request(
            upload_id,
            "0",
            4,
            Body::from_stream(futures_util::stream::once(async {
                Err::<bytes::Bytes, _>(io::Error::other("retry body must not be read"))
            })),
        ))
        .await
        .unwrap();
    assert_eq!(retry.status(), 204);
    assert_eq!(retry.headers()["upload-offset"], "4");

    let future = context
        .app()
        .oneshot(chunk_request(upload_id, "6", 2, Body::from("xx")))
        .await
        .unwrap();
    assert_eq!(future.status(), 409);
    assert_eq!(future.headers()["upload-offset"], "4");
    let (_, _, body) = json_response(future).await;
    assert_eq!(body["error"]["code"], "upload_offset_conflict");
    assert_eq!(body["error"]["details"]["expectedOffset"], "4");

    let exceeds_total = context
        .app()
        .oneshot(chunk_request(upload_id, "4", 9, Body::from("123456789")))
        .await
        .unwrap();
    assert_eq!(exceeds_total.status(), 409);
    assert_eq!(exceeds_total.headers()["upload-offset"], "4");
    assert_eq!(
        std::fs::read(
            context
                .temp
                .path()
                .join(".cellar/uploads")
                .join(format!("{upload_id}.part"))
        )
        .unwrap(),
        b"data"
    );
    context.close().await;
}

#[tokio::test]
async fn upload_chunk_rejects_inactive_and_unknown_sessions() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let failed_id = create_upload_session(&context, project_id, 4).await;
    context
        .database
        .mark_failed(failed_id, "test inactive")
        .await
        .unwrap();
    let finalizing_id = create_upload_session(&context, project_id, 0).await;
    assert!(
        context
            .database
            .mark_finalizing(finalizing_id, 0)
            .await
            .unwrap()
    );
    let complete_id = create_upload_session(&context, project_id, 0).await;
    assert!(
        context
            .database
            .mark_finalizing(complete_id, 0)
            .await
            .unwrap()
    );
    context.database.mark_complete(complete_id).await.unwrap();

    for upload_id in [failed_id, finalizing_id, complete_id] {
        let inactive = context
            .app()
            .oneshot(chunk_request(upload_id, "0", 0, Body::empty()))
            .await
            .unwrap();
        assert_eq!(inactive.status(), 409);
        assert_eq!(inactive.headers()["upload-offset"], "0");
    }

    let unknown = context
        .app()
        .oneshot(chunk_request(Uuid::now_v7(), "0", 1, Body::from("x")))
        .await
        .unwrap();
    let (status, _, body) = json_response(unknown).await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "upload_not_found");
    context.close().await;
}

#[tokio::test]
async fn upload_chunk_requires_canonical_path_and_exact_single_headers() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 20).await;

    let invalid_requests = [
        authenticated_request("PUT", &upload_chunk(upload_id))
            .header("origin", common::EXTERNAL_ORIGIN)
            .header("content-type", "application/octet-stream")
            .header("content-length", "1")
            .body(Body::from("x"))
            .unwrap(),
        authenticated_request("PUT", &upload_chunk(upload_id))
            .header("origin", common::EXTERNAL_ORIGIN)
            .header("upload-offset", "0")
            .header("content-length", "1")
            .body(Body::from("x"))
            .unwrap(),
        authenticated_request("PUT", &upload_chunk(upload_id))
            .header("origin", common::EXTERNAL_ORIGIN)
            .header("content-type", "application/octet-stream")
            .header("upload-offset", "0")
            .body(Body::from("x"))
            .unwrap(),
        authenticated_request("PUT", &upload_chunk(upload_id))
            .header("origin", common::EXTERNAL_ORIGIN)
            .header("content-type", "application/octet-stream; charset=binary")
            .header("upload-offset", "0")
            .header("content-length", "1")
            .body(Body::from("x"))
            .unwrap(),
        chunk_request(upload_id, "00", 1, Body::from("x")),
        chunk_request(upload_id, "+0", 1, Body::from("x")),
        chunk_request(upload_id, "9223372036854775808", 1, Body::from("x")),
        authenticated_request(
            "PUT",
            &format!(
                "/api/v1/uploads/{}/chunk",
                upload_id.to_string().to_uppercase()
            ),
        )
        .header("origin", common::EXTERNAL_ORIGIN)
        .header("content-type", "application/octet-stream")
        .header("upload-offset", "0")
        .header("content-length", "1")
        .body(Body::from("x"))
        .unwrap(),
    ];
    for request in invalid_requests {
        let response = context.app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 400);
    }

    let mut duplicate = chunk_request(upload_id, "0", 1, Body::from("x"));
    duplicate
        .headers_mut()
        .append("upload-offset", "0".parse().unwrap());
    assert_eq!(
        context.app().oneshot(duplicate).await.unwrap().status(),
        400
    );
    let mut duplicate_type = chunk_request(upload_id, "0", 1, Body::from("x"));
    duplicate_type
        .headers_mut()
        .append("content-type", "application/octet-stream".parse().unwrap());
    assert_eq!(
        context
            .app()
            .oneshot(duplicate_type)
            .await
            .unwrap()
            .status(),
        400
    );

    let mut duplicate_length = chunk_request(upload_id, "0", 1, Body::from("x"));
    duplicate_length
        .headers_mut()
        .append("content-length", "1".parse().unwrap());
    assert_eq!(
        context
            .app()
            .oneshot(duplicate_length)
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0)
    );
    context.close().await;
}

#[tokio::test]
async fn upload_chunk_route_remains_behind_authentication_and_exact_origin() {
    let context = TestContext::new().await;
    let upload_id = Uuid::now_v7();
    let unauthenticated = http::Request::builder()
        .method("PUT")
        .uri(upload_chunk(upload_id))
        .header("origin", common::EXTERNAL_ORIGIN)
        .header("content-type", "application/octet-stream")
        .header("upload-offset", "0")
        .header("content-length", "0")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        context
            .app()
            .oneshot(unauthenticated)
            .await
            .unwrap()
            .status(),
        401
    );
    let wrong_origin = authenticated_request("PUT", &upload_chunk(upload_id))
        .header("origin", "https://evil.example")
        .header("content-type", "application/octet-stream")
        .header("upload-offset", "0")
        .header("content-length", "0")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        context.app().oneshot(wrong_origin).await.unwrap().status(),
        403
    );
    context.close().await;
}

#[derive(Clone, Copy)]
enum ChunkStorageFault {
    FailAfterWrite,
    InsufficientSpace,
}

struct FaultyChunkStorage {
    storage: Arc<Storage>,
    fault: ChunkStorageFault,
    fail_rollback: bool,
}

impl UploadStorage for FaultyChunkStorage {
    fn destination_exists<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, bool> {
        Box::pin(async move { self.storage.destination_exists(project_id, file_name).await })
    }

    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.create_staging(upload_id).await })
    }

    fn remove_empty_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.remove_empty_staging(upload_id).await })
    }

    fn write_staging_chunk<'a>(
        &'a self,
        upload_id: Uuid,
        offset: u64,
        reader: cellar::uploads::UploadBodyReader,
    ) -> UploadStorageFuture<'a, u64> {
        Box::pin(async move {
            match self.fault {
                ChunkStorageFault::FailAfterWrite => {
                    self.storage.write_chunk(upload_id, offset, reader).await?;
                    Err(StorageError::Io {
                        source: io::Error::other("private simulated sync failure"),
                    })
                }
                ChunkStorageFault::InsufficientSpace => Err(StorageError::InsufficientSpace),
            }
        })
    }

    fn truncate_staging<'a>(&'a self, upload_id: Uuid, len: u64) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move {
            if self.fail_rollback {
                Err(StorageError::Io {
                    source: io::Error::other("private simulated rollback failure"),
                })
            } else {
                self.storage.truncate_staging(upload_id, len).await
            }
        })
    }
}

struct AdvanceFailingRepository {
    database: Database,
}

struct AdvanceAndRereadFailingRepository {
    database: Database,
    get_calls: AtomicUsize,
    commit_before_error: bool,
}

impl UploadRepository for AdvanceAndRereadFailingRepository {
    fn get_project<'a>(
        &'a self,
        project_id: Uuid,
    ) -> UploadRepositoryFuture<'a, Option<ProjectRow>> {
        Box::pin(async move { self.database.get_project(project_id).await })
    }

    fn create_upload<'a>(&'a self, upload: NewUpload) -> UploadRepositoryFuture<'a, UploadRow> {
        Box::pin(async move { self.database.create_upload(upload).await })
    }

    fn get_upload<'a>(&'a self, upload_id: Uuid) -> UploadRepositoryFuture<'a, Option<UploadRow>> {
        let call = self.get_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if call == 0 {
                self.database.get_upload(upload_id).await
            } else {
                Err(DbError::CorruptData)
            }
        })
    }

    fn advance_upload_offset<'a>(
        &'a self,
        upload_id: Uuid,
        expected: u64,
        next: u64,
    ) -> UploadRepositoryFuture<'a, bool> {
        Box::pin(async move {
            if self.commit_before_error {
                assert!(
                    self.database
                        .advance_offset(upload_id, expected, next)
                        .await?
                );
            }
            Err(DbError::CorruptData)
        })
    }

    fn mark_upload_failed<'a>(
        &'a self,
        upload_id: Uuid,
        reason: &'a str,
    ) -> UploadRepositoryFuture<'a, ()> {
        Box::pin(async move { self.database.mark_failed(upload_id, reason).await })
    }
}

struct RecordingRollbackStorage {
    storage: Arc<Storage>,
    rollback_calls: Arc<AtomicUsize>,
}

impl UploadStorage for RecordingRollbackStorage {
    fn destination_exists<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, bool> {
        Box::pin(async move { self.storage.destination_exists(project_id, file_name).await })
    }

    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.create_staging(upload_id).await })
    }

    fn remove_empty_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.remove_empty_staging(upload_id).await })
    }

    fn write_staging_chunk<'a>(
        &'a self,
        upload_id: Uuid,
        offset: u64,
        reader: cellar::uploads::UploadBodyReader,
    ) -> UploadStorageFuture<'a, u64> {
        Box::pin(async move { self.storage.write_chunk(upload_id, offset, reader).await })
    }

    fn truncate_staging<'a>(&'a self, upload_id: Uuid, len: u64) -> UploadStorageFuture<'a, ()> {
        self.rollback_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { self.storage.truncate_staging(upload_id, len).await })
    }

    fn staging_len<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, Option<u64>> {
        Box::pin(async move { self.storage.staging_len(upload_id).await })
    }
}

#[tokio::test]
async fn upload_chunk_advance_and_reread_failure_preserves_synced_ambiguous_bytes() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 10).await;
    let rollback_calls = Arc::new(AtomicUsize::new(0));
    let service = UploadService::new(
        Arc::new(AdvanceAndRereadFailingRepository {
            database: context.database.clone(),
            get_calls: AtomicUsize::new(0),
            commit_before_error: false,
        }),
        Arc::new(RecordingRollbackStorage {
            storage: context.storage.clone(),
            rollback_calls: rollback_calls.clone(),
        }),
    );
    let response = context
        .secure_upload(service)
        .oneshot(chunk_request(upload_id, "0", 4, Body::from("data")))
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert!(response.headers().get("upload-offset").is_none());
    assert_eq!(rollback_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(4)
    );
    let row = context
        .database
        .get_upload(upload_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.committed_offset(), 0);
    assert_eq!(row.state(), cellar::db::UploadState::Active);
    context.close().await;
}

#[tokio::test]
async fn upload_chunk_committed_advance_error_and_reread_failure_preserves_synced_commit() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 10).await;
    let rollback_calls = Arc::new(AtomicUsize::new(0));
    let service = UploadService::new(
        Arc::new(AdvanceAndRereadFailingRepository {
            database: context.database.clone(),
            get_calls: AtomicUsize::new(0),
            commit_before_error: true,
        }),
        Arc::new(RecordingRollbackStorage {
            storage: context.storage.clone(),
            rollback_calls: rollback_calls.clone(),
        }),
    );
    let response = context
        .secure_upload(service)
        .oneshot(chunk_request(upload_id, "0", 4, Body::from("data")))
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert!(response.headers().get("upload-offset").is_none());
    assert_eq!(rollback_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(4)
    );
    assert_eq!(
        std::fs::read(
            context
                .temp
                .path()
                .join(".cellar/uploads")
                .join(format!("{upload_id}.part"))
        )
        .unwrap(),
        b"data"
    );
    let row = context
        .database
        .get_upload(upload_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.committed_offset(), 4);
    assert_eq!(row.state(), cellar::db::UploadState::Active);
    context.close().await;
}

struct AmbiguousAdvanceRepository {
    database: Database,
}

impl UploadRepository for AmbiguousAdvanceRepository {
    fn get_project<'a>(
        &'a self,
        project_id: Uuid,
    ) -> UploadRepositoryFuture<'a, Option<ProjectRow>> {
        Box::pin(async move { self.database.get_project(project_id).await })
    }

    fn create_upload<'a>(&'a self, upload: NewUpload) -> UploadRepositoryFuture<'a, UploadRow> {
        Box::pin(async move { self.database.create_upload(upload).await })
    }

    fn get_upload<'a>(&'a self, upload_id: Uuid) -> UploadRepositoryFuture<'a, Option<UploadRow>> {
        Box::pin(async move { self.database.get_upload(upload_id).await })
    }

    fn advance_upload_offset<'a>(
        &'a self,
        upload_id: Uuid,
        expected: u64,
        next: u64,
    ) -> UploadRepositoryFuture<'a, bool> {
        Box::pin(async move {
            assert!(
                self.database
                    .advance_offset(upload_id, expected, next)
                    .await?
            );
            Ok(false)
        })
    }

    fn mark_upload_failed<'a>(
        &'a self,
        upload_id: Uuid,
        reason: &'a str,
    ) -> UploadRepositoryFuture<'a, ()> {
        Box::pin(async move { self.database.mark_failed(upload_id, reason).await })
    }
}

#[tokio::test]
async fn upload_chunk_ambiguous_conditional_advance_recovers_authoritative_commit() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 4).await;
    let service = UploadService::new(
        Arc::new(AmbiguousAdvanceRepository {
            database: context.database.clone(),
        }),
        context.storage.clone(),
    );
    let response = context
        .secure_upload(service)
        .oneshot(chunk_request(upload_id, "0", 4, Body::from("data")))
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert_eq!(response.headers()["upload-offset"], "4");
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(4)
    );
    assert_eq!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .unwrap()
            .committed_offset(),
        4
    );
    context.close().await;
}

impl UploadRepository for AdvanceFailingRepository {
    fn get_project<'a>(
        &'a self,
        project_id: Uuid,
    ) -> UploadRepositoryFuture<'a, Option<ProjectRow>> {
        Box::pin(async move { self.database.get_project(project_id).await })
    }

    fn create_upload<'a>(&'a self, upload: NewUpload) -> UploadRepositoryFuture<'a, UploadRow> {
        Box::pin(async move { self.database.create_upload(upload).await })
    }

    fn get_upload<'a>(&'a self, upload_id: Uuid) -> UploadRepositoryFuture<'a, Option<UploadRow>> {
        Box::pin(async move { self.database.get_upload(upload_id).await })
    }

    fn advance_upload_offset<'a>(
        &'a self,
        _: Uuid,
        _: u64,
        _: u64,
    ) -> UploadRepositoryFuture<'a, bool> {
        Box::pin(async { Err(DbError::CorruptData) })
    }

    fn mark_upload_failed<'a>(
        &'a self,
        upload_id: Uuid,
        reason: &'a str,
    ) -> UploadRepositoryFuture<'a, ()> {
        Box::pin(async move { self.database.mark_failed(upload_id, reason).await })
    }
}

#[tokio::test]
async fn upload_chunk_storage_stream_and_database_failures_never_advance_and_roll_back() {
    for fault in [
        ChunkStorageFault::FailAfterWrite,
        ChunkStorageFault::InsufficientSpace,
    ] {
        let context = TestContext::new().await;
        let project_id = create_project(&context).await;
        let upload_id = create_upload_session(&context, project_id, 10).await;
        let service = UploadService::new(
            Arc::new(context.database.clone()),
            Arc::new(FaultyChunkStorage {
                storage: context.storage.clone(),
                fault,
                fail_rollback: false,
            }),
        );
        let response = context
            .secure_upload(service)
            .oneshot(chunk_request(upload_id, "0", 4, Body::from("data")))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if matches!(fault, ChunkStorageFault::InsufficientSpace) {
                507
            } else {
                503
            }
        );
        assert_eq!(
            context.storage.staging_len(upload_id).await.unwrap(),
            Some(0)
        );
        assert_eq!(
            context
                .database
                .get_upload(upload_id)
                .await
                .unwrap()
                .unwrap()
                .committed_offset(),
            0
        );
        context.close().await;
    }

    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 10).await;
    let service = UploadService::new(
        Arc::new(AdvanceFailingRepository {
            database: context.database.clone(),
        }),
        context.storage.clone(),
    );
    let response = context
        .secure_upload(service)
        .oneshot(chunk_request(upload_id, "0", 4, Body::from("data")))
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers()["upload-offset"], "0");
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0)
    );
    assert_eq!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .unwrap()
            .committed_offset(),
        0
    );
    context.close().await;
}

#[tokio::test]
async fn upload_chunk_stream_error_rolls_back_exactly() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 10).await;
    let stream = futures_util::stream::iter([
        Ok::<_, io::Error>(bytes::Bytes::from_static(b"ab")),
        Err(io::Error::other("private body failure")),
    ]);
    let response = context
        .app()
        .oneshot(chunk_request(upload_id, "0", 4, Body::from_stream(stream)))
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(response.headers()["upload-offset"], "0");
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0)
    );
    assert_eq!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .unwrap()
            .committed_offset(),
        0
    );
    context.close().await;
}

#[tokio::test]
async fn upload_chunk_rollback_failure_marks_session_failed() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 10).await;
    let service = UploadService::new(
        Arc::new(context.database.clone()),
        Arc::new(FaultyChunkStorage {
            storage: context.storage.clone(),
            fault: ChunkStorageFault::FailAfterWrite,
            fail_rollback: true,
        }),
    );
    let response = context
        .secure_upload(service)
        .oneshot(chunk_request(upload_id, "0", 4, Body::from("data")))
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers()["upload-offset"], "0");
    let row = context
        .database
        .get_upload(upload_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.committed_offset(), 0);
    assert_eq!(row.state(), cellar::db::UploadState::Failed);
    assert_eq!(row.failure_reason(), Some("chunk rollback failed"));
    context.close().await;
}

struct MarkFailedFailingRepository {
    database: Database,
    mark_calls: Arc<AtomicUsize>,
}

impl UploadRepository for MarkFailedFailingRepository {
    fn get_project<'a>(
        &'a self,
        project_id: Uuid,
    ) -> UploadRepositoryFuture<'a, Option<ProjectRow>> {
        Box::pin(async move { self.database.get_project(project_id).await })
    }

    fn create_upload<'a>(&'a self, upload: NewUpload) -> UploadRepositoryFuture<'a, UploadRow> {
        Box::pin(async move { self.database.create_upload(upload).await })
    }

    fn get_upload<'a>(&'a self, upload_id: Uuid) -> UploadRepositoryFuture<'a, Option<UploadRow>> {
        Box::pin(async move { self.database.get_upload(upload_id).await })
    }

    fn advance_upload_offset<'a>(
        &'a self,
        upload_id: Uuid,
        expected: u64,
        next: u64,
    ) -> UploadRepositoryFuture<'a, bool> {
        Box::pin(async move {
            self.database
                .advance_offset(upload_id, expected, next)
                .await
        })
    }

    fn mark_upload_failed<'a>(&'a self, _: Uuid, _: &'a str) -> UploadRepositoryFuture<'a, ()> {
        self.mark_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(DbError::CorruptData) })
    }
}

#[tokio::test]
async fn upload_chunk_failed_state_transition_error_is_observed_and_logged_closed_safe() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 20).await;
    let logs = captured_logs();
    let mark_calls = Arc::new(AtomicUsize::new(0));
    let service = UploadService::new(
        Arc::new(MarkFailedFailingRepository {
            database: context.database.clone(),
            mark_calls: mark_calls.clone(),
        }),
        Arc::new(FaultyChunkStorage {
            storage: context.storage.clone(),
            fault: ChunkStorageFault::FailAfterWrite,
            fail_rollback: true,
        }),
    );
    let response = context
        .secure_upload(service)
        .oneshot(chunk_request(
            upload_id,
            "0",
            16,
            Body::from("SECRET_BODY_7391"),
        ))
        .await
        .unwrap();
    assert_eq!(response.headers()["upload-offset"], "0");
    let (status, request_id, body) = json_response(response).await;
    assert_eq!(status, 503);
    assert_eq!(mark_calls.load(Ordering::SeqCst), 1);
    assert_eq!(body["error"]["code"], "upload_chunk_failed");
    let row = context
        .database
        .get_upload(upload_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.committed_offset(), 0);
    assert_eq!(row.state(), cellar::db::UploadState::Active);
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(logs.lines().any(|line| {
        line.contains("failure_mark_failed")
            && line.contains(&request_id)
            && line.contains(&upload_id.to_string())
    }));
    for secret in [
        "private simulated rollback failure",
        "SECRET_BODY_7391",
        OWNER_EMAIL,
    ] {
        assert!(!logs.contains(secret));
        assert!(!body.to_string().contains(secret));
    }
    context.close().await;
}

#[tokio::test]
async fn concurrent_upload_chunks_at_zero_serialize_without_interleaving_or_duplication() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 4).await;
    let app = context.app();
    let barrier = Arc::new(Barrier::new(3));
    let first = {
        let app = app.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            app.oneshot(chunk_request(upload_id, "0", 4, Body::from("AAAA")))
                .await
                .unwrap()
        })
    };
    let second = {
        let app = app.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            app.oneshot(chunk_request(upload_id, "0", 4, Body::from("BBBB")))
                .await
                .unwrap()
        })
    };
    barrier.wait().await;
    let (first, second) = tokio::join!(first, second);
    assert_eq!(first.unwrap().status(), 204);
    assert_eq!(second.unwrap().status(), 204);
    let bytes = std::fs::read(
        context
            .temp
            .path()
            .join(".cellar/uploads")
            .join(format!("{upload_id}.part")),
    )
    .unwrap();
    assert!(bytes == b"AAAA" || bytes == b"BBBB", "stored {bytes:?}");
    assert_eq!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .unwrap()
            .committed_offset(),
        4
    );
    context.close().await;
}

struct PausingChunkStorage {
    storage: Arc<Storage>,
    written: Mutex<Option<oneshot::Sender<()>>>,
    release: Arc<Semaphore>,
}

impl UploadStorage for PausingChunkStorage {
    fn destination_exists<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, bool> {
        Box::pin(async move { self.storage.destination_exists(project_id, file_name).await })
    }

    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.create_staging(upload_id).await })
    }

    fn remove_empty_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.remove_empty_staging(upload_id).await })
    }

    fn write_staging_chunk<'a>(
        &'a self,
        upload_id: Uuid,
        offset: u64,
        reader: cellar::uploads::UploadBodyReader,
    ) -> UploadStorageFuture<'a, u64> {
        let written = self.written.lock().unwrap().take();
        Box::pin(async move {
            let count = self.storage.write_chunk(upload_id, offset, reader).await?;
            if let Some(written) = written {
                let _ = written.send(());
            }
            self.release.acquire().await.unwrap().forget();
            Ok(count)
        })
    }

    fn truncate_staging<'a>(&'a self, upload_id: Uuid, len: u64) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.truncate_staging(upload_id, len).await })
    }
}

#[tokio::test]
async fn cancelled_chunk_request_finishes_owned_commit_and_retry_cannot_duplicate() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let upload_id = create_upload_session(&context, project_id, 4).await;
    let (written_tx, written_rx) = oneshot::channel();
    let release = Arc::new(Semaphore::new(0));
    let service = UploadService::new(
        Arc::new(context.database.clone()),
        Arc::new(PausingChunkStorage {
            storage: context.storage.clone(),
            written: Mutex::new(Some(written_tx)),
            release: release.clone(),
        }),
    );
    let app = context.secure_upload(service);
    let request =
        tokio::spawn(
            app.clone()
                .oneshot(chunk_request(upload_id, "0", 4, Body::from("data"))),
        );
    written_rx.await.unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    release.add_permits(1);

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if context
                .database
                .get_upload(upload_id)
                .await
                .unwrap()
                .unwrap()
                .committed_offset()
                == 4
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned chunk operation did not finish after cancellation");

    let retry = app
        .oneshot(chunk_request(upload_id, "0", 4, Body::from("EVIL")))
        .await
        .unwrap();
    assert_eq!(retry.status(), 204);
    assert_eq!(retry.headers()["upload-offset"], "4");
    assert_eq!(
        std::fs::read(
            context
                .temp
                .path()
                .join(".cellar/uploads")
                .join(format!("{upload_id}.part"))
        )
        .unwrap(),
        b"data"
    );
    context.close().await;
}

#[test]
fn upload_session_decimal_wire_type_accepts_digits_and_normalizes_leading_zeros() {
    let parsed: DecimalU64 = serde_json::from_str(r#""000150""#).unwrap();
    assert_eq!(parsed, DecimalU64(150));
    assert_eq!(serde_json::to_string(&parsed).unwrap(), r#""150""#);
}

#[test]
fn upload_session_decimal_wire_type_rejects_noncanonical_forms_and_out_of_range() {
    for invalid in [
        r#"""#,
        r#""+1""#,
        r#""-1""#,
        r#"" 1""#,
        r#""1 ""#,
        r#""1e3""#,
        "1",
        "1.0",
        r#""9223372036854775808""#,
    ] {
        assert!(
            serde_json::from_str::<DecimalU64>(invalid).is_err(),
            "accepted {invalid}"
        );
    }
    let maximum: DecimalU64 = serde_json::from_str(r#""9223372036854775807""#).unwrap();
    assert_eq!(maximum, DecimalU64(i64::MAX as u64));
}

#[tokio::test]
async fn upload_session_create_and_status_have_exact_contract_and_persistence() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let response = context
        .app()
        .oneshot(json_request(
            authenticated_write_request(&uploads(project_id)),
            r#"{"fileName":"archive.zip","totalSize":"150000000"}"#,
        ))
        .await
        .unwrap();
    let (status, _, body) = json_response(response).await;
    assert_eq!(status, 201);
    assert_eq!(body.as_object().unwrap().len(), 6);
    let upload_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    assert_eq!(upload_id.get_version(), Some(Version::SortRand));
    assert_eq!(
        body,
        json!({
            "id": upload_id,
            "projectId": project_id,
            "fileName": "archive.zip",
            "totalSize": "150000000",
            "committedOffset": "0",
            "state": "active"
        })
    );
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0)
    );
    let row = context
        .database
        .get_upload(upload_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.project_id(), project_id);
    assert_eq!(row.file_name(), "archive.zip");
    assert_eq!(row.total_size(), 150_000_000);
    assert_eq!(row.committed_offset(), 0);

    context.storage.remove_staging(upload_id).await.unwrap();

    let response = context
        .app()
        .oneshot(
            authenticated_request("GET", &upload(upload_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _, status_body) = json_response(response).await;
    assert_eq!(status, 200);
    assert_eq!(status_body, body);
    assert_eq!(context.storage.staging_len(upload_id).await.unwrap(), None);
    context.close().await;
}

#[tokio::test]
async fn upload_session_leading_zero_size_is_accepted_and_normalized() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let response = context
        .app()
        .oneshot(json_request(
            authenticated_write_request(&uploads(project_id)),
            r#"{"fileName":"small.bin","totalSize":"00010"}"#,
        ))
        .await
        .unwrap();
    let (status, _, body) = json_response(response).await;
    assert_eq!(status, 201);
    assert_eq!(body["totalSize"], "10");
    context.close().await;
}

#[tokio::test]
async fn upload_session_unknown_project_and_upload_are_stable_404s() {
    let context = TestContext::new().await;
    let missing = Uuid::parse_str("0198f67e-9c0b-7000-8000-000000000801").unwrap();
    let create = context
        .app()
        .oneshot(json_request(
            authenticated_write_request(&uploads(missing)),
            r#"{"fileName":"archive.zip","totalSize":"1"}"#,
        ))
        .await
        .unwrap();
    let (status, request_id, body) = json_response(create).await;
    assert_eq!(status, 404);
    assert_eq!(
        body,
        json!({"error": {
            "code": "project_not_found",
            "message": "The project was not found.",
            "requestId": request_id
        }})
    );

    let status_response = context
        .app()
        .oneshot(
            authenticated_request("GET", &upload(missing))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, request_id, body) = json_response(status_response).await;
    assert_eq!(status, 404);
    assert_eq!(
        body,
        json!({"error": {
            "code": "upload_not_found",
            "message": "The upload was not found.",
            "requestId": request_id
        }})
    );
    context.close().await;
}

#[tokio::test]
async fn upload_session_directory_without_committed_project_is_still_404() {
    let context = TestContext::new().await;
    let project_id = Uuid::now_v7();
    context
        .storage
        .create_project_dir(project_id)
        .await
        .unwrap();
    let response = context
        .app()
        .oneshot(json_request(
            authenticated_write_request(&uploads(project_id)),
            r#"{"fileName":"archive.zip","totalSize":"1"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert!(
        context
            .database
            .recoverable_uploads()
            .await
            .unwrap()
            .is_empty()
    );
    context.close().await;
}

#[tokio::test]
async fn upload_session_noncanonical_uuid_paths_are_stable_400s() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let uppercase_project = project_id.to_string().to_ascii_uppercase();
    let create = context
        .app()
        .oneshot(json_request(
            authenticated_write_request(&format!("/api/v1/projects/{uppercase_project}/uploads")),
            r#"{"fileName":"archive.zip","totalSize":"1"}"#,
        ))
        .await
        .unwrap();
    let (status, _, body) = json_response(create).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");

    let upload_id = Uuid::now_v7();
    let get = context
        .app()
        .oneshot(
            authenticated_request("GET", &format!("/api/v1/uploads/urn:uuid:{upload_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _, body) = json_response(get).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");
    assert!(
        context
            .database
            .recoverable_uploads()
            .await
            .unwrap()
            .is_empty()
    );
    context.close().await;
}

#[tokio::test]
async fn upload_session_invalid_filename_size_and_body_are_stable_400s() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let cases = [
        r#"{"fileName":"../escape","totalSize":"1"}"#,
        r#"{"fileName":"CON","totalSize":"1"}"#,
        r#"{"fileName":"","totalSize":"1"}"#,
        r#"{"fileName":"ok.bin","totalSize":""}"#,
        r#"{"fileName":"ok.bin","totalSize":"+1"}"#,
        r#"{"fileName":"ok.bin","totalSize":"-1"}"#,
        r#"{"fileName":"ok.bin","totalSize":" 1"}"#,
        r#"{"fileName":"ok.bin","totalSize":"1e3"}"#,
        r#"{"fileName":"ok.bin","totalSize":1}"#,
        r#"{"fileName":"ok.bin","totalSize":"9223372036854775808"}"#,
        r#"{"fileName":"ok.bin","totalSize":"1","extra":true}"#,
        r#"{"fileName":"ok.bin"}"#,
        r#"[]"#,
        r#"{"fileName":"broken""#,
    ];
    for request_body in cases {
        let response = context
            .app()
            .oneshot(json_request(
                authenticated_write_request(&uploads(project_id)),
                request_body,
            ))
            .await
            .unwrap();
        let (status, request_id, body) = json_response(response).await;
        assert_eq!(status, 400, "body {request_body}");
        assert_eq!(
            body,
            json!({"error": {
                "code": "invalid_request",
                "message": "The request is invalid.",
                "requestId": request_id
            }}),
            "body {request_body}"
        );
    }
    assert!(
        context
            .database
            .recoverable_uploads()
            .await
            .unwrap()
            .is_empty()
    );
    context.close().await;
}

#[tokio::test]
async fn upload_session_existing_final_file_is_409_without_staging_or_row() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    std::fs::write(
        context
            .project_path(project_id)
            .join("files")
            .join("same.txt"),
        b"original",
    )
    .unwrap();
    let response = context
        .app()
        .oneshot(json_request(
            authenticated_write_request(&uploads(project_id)),
            r#"{"fileName":"same.txt","totalSize":"8"}"#,
        ))
        .await
        .unwrap();
    let (status, request_id, body) = json_response(response).await;
    assert_eq!(status, 409);
    assert_eq!(
        body,
        json!({"error": {
            "code": "destination_exists",
            "message": "A file with that name already exists.",
            "requestId": request_id
        }})
    );
    assert_eq!(
        std::fs::read(
            context
                .project_path(project_id)
                .join("files")
                .join("same.txt")
        )
        .unwrap(),
        b"original"
    );
    assert!(
        context
            .database
            .recoverable_uploads()
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        std::fs::read_dir(context.temp.path().join(".cellar").join("uploads"))
            .unwrap()
            .count(),
        0
    );
    context.close().await;
}

#[tokio::test]
async fn upload_session_routes_are_authenticated_and_origin_protected() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let path = uploads(project_id);
    for request in [
        http::Request::post(&path).body(Body::empty()).unwrap(),
        authenticated_request("POST", &path)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"fileName":"x","totalSize":"1"}"#))
            .unwrap(),
        authenticated_request("POST", &path)
            .header("origin", "https://evil.example")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"fileName":"x","totalSize":"1"}"#))
            .unwrap(),
    ] {
        let response = context.app().oneshot(request).await.unwrap();
        assert!(response.status() == 401 || response.status() == 403);
    }
    assert!(
        context
            .database
            .recoverable_uploads()
            .await
            .unwrap()
            .is_empty()
    );

    let missing_assertion = context
        .app()
        .oneshot(
            http::Request::get(upload(Uuid::now_v7()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_assertion.status(), 401);
    context.close().await;
}

#[derive(Clone)]
struct CreateFailingRepository {
    database: Database,
}

impl UploadRepository for CreateFailingRepository {
    fn get_project<'a>(
        &'a self,
        project_id: Uuid,
    ) -> UploadRepositoryFuture<'a, Option<ProjectRow>> {
        Box::pin(async move { self.database.get_project(project_id).await })
    }

    fn create_upload<'a>(&'a self, _: NewUpload) -> UploadRepositoryFuture<'a, UploadRow> {
        Box::pin(async { Err(DbError::Conflict) })
    }

    fn get_upload<'a>(&'a self, upload_id: Uuid) -> UploadRepositoryFuture<'a, Option<UploadRow>> {
        Box::pin(async move { self.database.get_upload(upload_id).await })
    }
}

#[tokio::test]
async fn upload_session_database_failure_removes_exact_staging_and_preserves_siblings() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let sibling = context
        .temp
        .path()
        .join(".cellar")
        .join("uploads")
        .join("unrelated.part");
    std::fs::write(&sibling, b"keep").unwrap();
    let service = UploadService::new(
        Arc::new(CreateFailingRepository {
            database: context.database.clone(),
        }),
        context.storage.clone(),
    );
    let response = context
        .secure_upload(service)
        .oneshot(json_request(
            authenticated_write_request(&uploads(project_id)),
            r#"{"fileName":"archive.zip","totalSize":"10"}"#,
        ))
        .await
        .unwrap();
    let (status, request_id, body) = json_response(response).await;
    assert_eq!(status, 503);
    assert_eq!(
        body,
        json!({"error": {
            "code": "upload_create_failed",
            "message": "Upload creation is temporarily unavailable.",
            "requestId": request_id
        }})
    );
    assert_eq!(std::fs::read(&sibling).unwrap(), b"keep");
    let entries = std::fs::read_dir(sibling.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(entries, vec!["unrelated.part"]);
    assert!(
        context
            .database
            .recoverable_uploads()
            .await
            .unwrap()
            .is_empty()
    );
    context.close().await;
}

struct CleanupFailingStorage {
    storage: Arc<Storage>,
    created_id: Arc<Mutex<Option<Uuid>>>,
    private_path: String,
}

struct RecordingStorage {
    storage: Arc<Storage>,
    created_id: Arc<Mutex<Option<Uuid>>>,
    cleanup_calls: Arc<AtomicUsize>,
    cleanup_fails: bool,
}

impl UploadStorage for RecordingStorage {
    fn destination_exists<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, bool> {
        Box::pin(async move { self.storage.destination_exists(project_id, file_name).await })
    }

    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move {
            self.storage.create_staging(upload_id).await?;
            *self.created_id.lock().unwrap() = Some(upload_id);
            Ok(())
        })
    }

    fn remove_empty_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        self.cleanup_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.cleanup_fails {
                Err(StorageError::Io {
                    source: io::Error::new(io::ErrorKind::PermissionDenied, "private-path"),
                })
            } else {
                self.storage.remove_empty_staging(upload_id).await
            }
        })
    }
}

struct PanickingRepository {
    database: Database,
    panic_after_commit: bool,
    reconciliation_fails: bool,
    reconciliation_panics: bool,
    reconciliation_calls: Arc<AtomicUsize>,
}

impl UploadRepository for PanickingRepository {
    fn get_project<'a>(
        &'a self,
        project_id: Uuid,
    ) -> UploadRepositoryFuture<'a, Option<ProjectRow>> {
        Box::pin(async move { self.database.get_project(project_id).await })
    }

    fn create_upload<'a>(&'a self, upload: NewUpload) -> UploadRepositoryFuture<'a, UploadRow> {
        Box::pin(async move {
            if self.panic_after_commit {
                self.database.create_upload(upload).await.unwrap();
            }
            panic!("deterministic repository panic")
        })
    }

    fn get_upload<'a>(&'a self, upload_id: Uuid) -> UploadRepositoryFuture<'a, Option<UploadRow>> {
        self.reconciliation_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            assert!(
                !self.reconciliation_panics,
                "deterministic reconciliation panic"
            );
            if self.reconciliation_fails {
                Err(DbError::CorruptData)
            } else {
                self.database.get_upload(upload_id).await
            }
        })
    }
}

struct PanicTestRig {
    service: UploadService,
    created_id: Arc<Mutex<Option<Uuid>>>,
    cleanup_calls: Arc<AtomicUsize>,
    reconciliation_calls: Arc<AtomicUsize>,
}

fn panic_test_service(
    context: &TestContext,
    panic_after_commit: bool,
    reconciliation_fails: bool,
    reconciliation_panics: bool,
    cleanup_fails: bool,
) -> PanicTestRig {
    let created_id = Arc::new(Mutex::new(None));
    let cleanup_calls = Arc::new(AtomicUsize::new(0));
    let reconciliation_calls = Arc::new(AtomicUsize::new(0));
    let repository = PanickingRepository {
        database: context.database.clone(),
        panic_after_commit,
        reconciliation_fails,
        reconciliation_panics,
        reconciliation_calls: reconciliation_calls.clone(),
    };
    let storage = RecordingStorage {
        storage: context.storage.clone(),
        created_id: created_id.clone(),
        cleanup_calls: cleanup_calls.clone(),
        cleanup_fails,
    };
    PanicTestRig {
        service: UploadService::new(Arc::new(repository), Arc::new(storage)),
        created_id,
        cleanup_calls,
        reconciliation_calls,
    }
}

#[tokio::test]
async fn upload_session_panic_after_staging_before_commit_compensates_empty_file() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let rig = panic_test_service(&context, false, false, false, false);
    let response = context
        .secure_upload(rig.service)
        .oneshot(json_request(
            authenticated_write_request(&uploads(project_id)),
            r#"{"fileName":"before.bin","totalSize":"10"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let upload_id = rig.created_id.lock().unwrap().unwrap();
    assert_eq!(rig.reconciliation_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.cleanup_calls.load(Ordering::SeqCst), 1);
    assert_eq!(context.storage.staging_len(upload_id).await.unwrap(), None);
    assert!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .is_none()
    );
    context.close().await;
}

#[tokio::test]
async fn upload_session_panic_after_database_commit_returns_identified_live_session() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let rig = panic_test_service(&context, true, false, false, false);
    let response = context
        .secure_upload(rig.service)
        .oneshot(json_request(
            authenticated_write_request(&uploads(project_id)),
            r#"{"fileName":"after.bin","totalSize":"10"}"#,
        ))
        .await
        .unwrap();
    let (status, _, body) = json_response(response).await;
    assert_eq!(status, 201);
    let upload_id = rig.created_id.lock().unwrap().unwrap();
    assert_eq!(body["id"], upload_id.to_string());
    assert_eq!(rig.reconciliation_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.cleanup_calls.load(Ordering::SeqCst), 0);
    assert!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0)
    );
    context.close().await;
}

#[tokio::test]
async fn upload_session_failed_panic_reconciliation_preserves_ambiguous_staging() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let logs = captured_logs();
    let rig = panic_test_service(&context, false, true, false, false);
    let response = context
        .secure_upload(rig.service)
        .oneshot(json_request(
            authenticated_write_request(&uploads(project_id)),
            r#"{"fileName":"ambiguous.bin","totalSize":"10"}"#,
        ))
        .await
        .unwrap();
    let (status, request_id, _) = json_response(response).await;
    assert_eq!(status, 503);
    let upload_id = rig.created_id.lock().unwrap().unwrap();
    assert_eq!(rig.reconciliation_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.cleanup_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0)
    );
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(logs.lines().any(|line| {
        line.contains("database_reconciliation_failed")
            && line.contains(&request_id)
            && line.contains(&upload_id.to_string())
            && line.contains(&project_id.to_string())
    }));
    assert!(!logs.contains(context.temp.path().to_string_lossy().as_ref()));
    context.close().await;
}

#[tokio::test]
async fn upload_session_panic_cleanup_failure_returns_safe_cleanup_503() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let rig = panic_test_service(&context, false, false, false, true);
    let response = context
        .secure_upload(rig.service)
        .oneshot(json_request(
            authenticated_write_request(&uploads(project_id)),
            r#"{"fileName":"cleanup.bin","totalSize":"10"}"#,
        ))
        .await
        .unwrap();
    let (status, request_id, body) = json_response(response).await;
    assert_eq!(status, 503);
    assert_eq!(
        body,
        json!({"error": {
            "code": "upload_cleanup_failed",
            "message": "Upload creation could not be completed safely.",
            "requestId": request_id
        }})
    );
    let upload_id = rig.created_id.lock().unwrap().unwrap();
    assert_eq!(rig.reconciliation_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.cleanup_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0)
    );
    context.close().await;
}

#[tokio::test]
async fn upload_session_reconciliation_task_panic_is_safe_and_preserves_staging() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let logs = captured_logs();
    let rig = panic_test_service(&context, false, false, true, false);
    let response = context
        .secure_upload(rig.service)
        .oneshot(json_request(
            authenticated_write_request(&uploads(project_id)),
            r#"{"fileName":"reconcile-panic.bin","totalSize":"10"}"#,
        ))
        .await
        .unwrap();
    let (status, request_id, _) = json_response(response).await;
    assert_eq!(status, 503);
    let upload_id = rig.created_id.lock().unwrap().unwrap();
    assert_eq!(rig.reconciliation_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.cleanup_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0)
    );
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(logs.lines().any(|line| {
        line.contains("reconciliation_task_failed")
            && line.contains(&request_id)
            && line.contains(&upload_id.to_string())
            && line.contains(&project_id.to_string())
    }));
    assert!(!logs.contains(context.temp.path().to_string_lossy().as_ref()));
    context.close().await;
}

struct PausingReconciliationRepository {
    database: Database,
    lookup_started: Mutex<Option<oneshot::Sender<()>>>,
    release: Arc<Semaphore>,
}

struct PausingCreatePanicRepository {
    database: Database,
    create_started: Mutex<Option<oneshot::Sender<()>>>,
    release: Arc<Semaphore>,
}

impl UploadRepository for PausingCreatePanicRepository {
    fn get_project<'a>(
        &'a self,
        project_id: Uuid,
    ) -> UploadRepositoryFuture<'a, Option<ProjectRow>> {
        Box::pin(async move { self.database.get_project(project_id).await })
    }

    fn create_upload<'a>(&'a self, _: NewUpload) -> UploadRepositoryFuture<'a, UploadRow> {
        let create_started = self.create_started.lock().unwrap().take();
        Box::pin(async move {
            if let Some(create_started) = create_started {
                let _ = create_started.send(());
            }
            self.release.acquire().await.unwrap().forget();
            panic!("deterministic delayed create panic")
        })
    }

    fn get_upload<'a>(&'a self, upload_id: Uuid) -> UploadRepositoryFuture<'a, Option<UploadRow>> {
        Box::pin(async move { self.database.get_upload(upload_id).await })
    }
}

#[tokio::test]
async fn upload_session_request_abort_before_create_join_error_still_reconciles() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let (create_started_tx, create_started_rx) = oneshot::channel();
    let release = Arc::new(Semaphore::new(0));
    let created_id = Arc::new(Mutex::new(None));
    let cleanup_calls = Arc::new(AtomicUsize::new(0));
    let repository = PausingCreatePanicRepository {
        database: context.database.clone(),
        create_started: Mutex::new(Some(create_started_tx)),
        release: release.clone(),
    };
    let storage = RecordingStorage {
        storage: context.storage.clone(),
        created_id: created_id.clone(),
        cleanup_calls: cleanup_calls.clone(),
        cleanup_fails: false,
    };
    let app = context.secure_upload(UploadService::new(Arc::new(repository), Arc::new(storage)));
    let request = tokio::spawn(app.oneshot(json_request(
        authenticated_write_request(&uploads(project_id)),
        r#"{"fileName":"cancel-before-join.bin","totalSize":"10"}"#,
    )));
    tokio::time::timeout(std::time::Duration::from_secs(2), create_started_rx)
        .await
        .unwrap()
        .unwrap();
    let upload_id = created_id.lock().unwrap().unwrap();
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0)
    );

    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    release.add_permits(1);

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if context
                .storage
                .staging_len(upload_id)
                .await
                .unwrap()
                .is_none()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached create supervisor did not reconcile staging");
    assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
    assert!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .is_none()
    );
    context.close().await;
}

impl UploadRepository for PausingReconciliationRepository {
    fn get_project<'a>(
        &'a self,
        project_id: Uuid,
    ) -> UploadRepositoryFuture<'a, Option<ProjectRow>> {
        Box::pin(async move { self.database.get_project(project_id).await })
    }

    fn create_upload<'a>(&'a self, _: NewUpload) -> UploadRepositoryFuture<'a, UploadRow> {
        Box::pin(async { panic!("deterministic create panic before commit") })
    }

    fn get_upload<'a>(&'a self, upload_id: Uuid) -> UploadRepositoryFuture<'a, Option<UploadRow>> {
        let lookup_started = self.lookup_started.lock().unwrap().take();
        Box::pin(async move {
            if let Some(lookup_started) = lookup_started {
                let _ = lookup_started.send(());
            }
            self.release.acquire().await.unwrap().forget();
            self.database.get_upload(upload_id).await
        })
    }
}

#[tokio::test]
async fn upload_session_request_abort_during_reconciliation_still_compensates() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let (lookup_started_tx, lookup_started_rx) = oneshot::channel();
    let release = Arc::new(Semaphore::new(0));
    let created_id = Arc::new(Mutex::new(None));
    let cleanup_calls = Arc::new(AtomicUsize::new(0));
    let repository = PausingReconciliationRepository {
        database: context.database.clone(),
        lookup_started: Mutex::new(Some(lookup_started_tx)),
        release: release.clone(),
    };
    let storage = RecordingStorage {
        storage: context.storage.clone(),
        created_id: created_id.clone(),
        cleanup_calls: cleanup_calls.clone(),
        cleanup_fails: false,
    };
    let app = context.secure_upload(UploadService::new(Arc::new(repository), Arc::new(storage)));
    let request = tokio::spawn(app.oneshot(json_request(
        authenticated_write_request(&uploads(project_id)),
        r#"{"fileName":"cancel-reconcile.bin","totalSize":"10"}"#,
    )));
    tokio::time::timeout(std::time::Duration::from_secs(2), lookup_started_rx)
        .await
        .unwrap()
        .unwrap();
    let upload_id = created_id.lock().unwrap().unwrap();
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0)
    );

    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    release.add_permits(1);

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if context
                .storage
                .staging_len(upload_id)
                .await
                .unwrap()
                .is_none()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached reconciliation did not compensate staging");
    assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
    assert!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .is_none()
    );
    context.close().await;
}

struct FullStorage;

impl UploadStorage for FullStorage {
    fn destination_exists<'a>(
        &'a self,
        _: Uuid,
        _: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, bool> {
        Box::pin(async { Ok(false) })
    }

    fn create_staging<'a>(&'a self, _: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async { Err(StorageError::InsufficientSpace) })
    }

    fn remove_empty_staging<'a>(&'a self, _: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn upload_session_storage_capacity_failure_is_507_without_database_row() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let service = UploadService::new(Arc::new(context.database.clone()), Arc::new(FullStorage));
    let response = context
        .secure_upload(service)
        .oneshot(json_request(
            authenticated_write_request(&uploads(project_id)),
            r#"{"fileName":"large.bin","totalSize":"9223372036854775807"}"#,
        ))
        .await
        .unwrap();
    let (status, request_id, body) = json_response(response).await;
    assert_eq!(status, 507);
    assert_eq!(body["error"]["code"], "insufficient_storage");
    assert_eq!(body["error"]["requestId"], request_id);
    assert!(
        context
            .database
            .recoverable_uploads()
            .await
            .unwrap()
            .is_empty()
    );
    context.close().await;
}

impl UploadStorage for CleanupFailingStorage {
    fn destination_exists<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, bool> {
        Box::pin(async move { self.storage.destination_exists(project_id, file_name).await })
    }

    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move {
            self.storage.create_staging(upload_id).await?;
            *self.created_id.lock().unwrap() = Some(upload_id);
            Ok(())
        })
    }

    fn remove_empty_staging<'a>(&'a self, _: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move {
            Err(StorageError::Io {
                source: io::Error::new(io::ErrorKind::PermissionDenied, self.private_path.clone()),
            })
        })
    }
}

#[derive(Clone)]
struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

static CAPTURED_LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();

fn captured_logs() -> Arc<Mutex<Vec<u8>>> {
    CAPTURED_LOGS
        .get_or_init(|| {
            let logs = Arc::new(Mutex::new(Vec::new()));
            let log_writer = CapturedWriter(logs.clone());
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || log_writer.clone())
                .finish();
            tracing::subscriber::set_global_default(subscriber).unwrap();
            logs
        })
        .clone()
}

#[tokio::test]
async fn upload_session_cleanup_failure_returns_safe_503_and_closed_log() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let logs = captured_logs();
    let created_id = Arc::new(Mutex::new(None));
    let private_path = context.temp.path().join("secret").display().to_string();
    let service = UploadService::new(
        Arc::new(CreateFailingRepository {
            database: context.database.clone(),
        }),
        Arc::new(CleanupFailingStorage {
            storage: context.storage.clone(),
            created_id: created_id.clone(),
            private_path: private_path.clone(),
        }),
    );
    let response = context
        .secure_upload(service)
        .oneshot(json_request(
            authenticated_write_request(&uploads(project_id)),
            r#"{"fileName":"secret-name.txt","totalSize":"10"}"#,
        ))
        .await
        .unwrap();
    let (status, request_id, body) = json_response(response).await;
    assert_eq!(status, 503);
    assert_eq!(
        body,
        json!({"error": {
            "code": "upload_cleanup_failed",
            "message": "Upload creation could not be completed safely.",
            "requestId": request_id
        }})
    );
    let upload_id = created_id.lock().unwrap().unwrap();
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(
        logs.lines().any(|line| {
            line.contains("staging_cleanup_failed")
                && line.contains(&request_id)
                && line.contains(&upload_id.to_string())
                && line.contains(&project_id.to_string())
        }),
        "missing closed cleanup event in {logs}"
    );
    for secret in [
        private_path.as_str(),
        "secret-name.txt",
        OWNER_EMAIL,
        VALID_ASSERTION,
        "database constraint conflict",
    ] {
        assert!(!body.to_string().contains(secret));
        assert!(!logs.contains(secret), "logs contained {secret}");
    }
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0)
    );
    assert!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .is_none()
    );
    context.close().await;
}

struct PausingStagingStorage {
    storage: Arc<Storage>,
    created: Mutex<Option<oneshot::Sender<Uuid>>>,
    release: Arc<Semaphore>,
}

impl UploadStorage for PausingStagingStorage {
    fn destination_exists<'a>(
        &'a self,
        project_id: Uuid,
        file_name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, bool> {
        Box::pin(async move { self.storage.destination_exists(project_id, file_name).await })
    }

    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        let created = self.created.lock().unwrap().take();
        Box::pin(async move {
            self.storage.create_staging(upload_id).await?;
            if let Some(created) = created {
                let _ = created.send(upload_id);
            }
            self.release.acquire().await.unwrap().forget();
            Ok(())
        })
    }

    fn remove_empty_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.remove_empty_staging(upload_id).await })
    }
}

#[tokio::test]
async fn upload_session_abort_after_staging_creation_finishes_owned_operation() {
    let context = TestContext::new().await;
    let project_id = create_project(&context).await;
    let (created_tx, created_rx) = oneshot::channel();
    let release = Arc::new(Semaphore::new(0));
    let storage = Arc::new(PausingStagingStorage {
        storage: context.storage.clone(),
        created: Mutex::new(Some(created_tx)),
        release: release.clone(),
    });
    let app = context.secure_upload(UploadService::new(
        Arc::new(context.database.clone()),
        storage,
    ));
    let request = tokio::spawn(app.oneshot(json_request(
        authenticated_write_request(&uploads(project_id)),
        r#"{"fileName":"cancel.bin","totalSize":"10"}"#,
    )));
    let upload_id = tokio::time::timeout(std::time::Duration::from_secs(2), created_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0)
    );

    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    release.add_permits(1);

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if context
                .database
                .get_upload(upload_id)
                .await
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned upload creation did not finish after request cancellation");
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0)
    );
    context.close().await;
}

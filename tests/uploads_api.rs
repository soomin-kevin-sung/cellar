mod common;

use std::{
    io::{self, Write},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
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
use tokio::sync::{Semaphore, oneshot};
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

#[tokio::test]
async fn upload_session_chunk_put_is_not_implemented() {
    let context = TestContext::new().await;
    let upload_id = Uuid::now_v7();
    let response = context
        .app()
        .oneshot(
            authenticated_request("PUT", &format!("{}/chunk", upload(upload_id)))
                .header("origin", common::EXTERNAL_ORIGIN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(context.storage.staging_len(upload_id).await.unwrap(), None);
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

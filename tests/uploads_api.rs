mod common;

use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use bytes::Bytes;
use cellar::{
    db::{DbError, ProjectRow},
    storage::{SafeFileName, StorageError},
    uploads::{
        UploadBodyReader, UploadFuture, UploadRepository, UploadService, UploadStorage,
        UploadStorageFuture,
    },
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use common::{
    ASSERTION_HEADER, EXTERNAL_ORIGIN, TestContext, VALID_ASSERTION, authenticated_request,
    json_response,
};

fn upload_uri(project_id: impl std::fmt::Display, file_name_query: &str) -> String {
    format!("/api/v1/projects/{project_id}/uploads?{file_name_query}")
}

fn upload_request(
    project_id: impl std::fmt::Display,
    file_name_query: &str,
    body: Body,
) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(upload_uri(project_id, file_name_query))
        .header("origin", EXTERNAL_ORIGIN)
        .header(ASSERTION_HEADER, VALID_ASSERTION)
        .header("content-type", "application/octet-stream")
        .body(body)
        .unwrap()
}

async fn response_json(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

fn canonical_staging_files(context: &TestContext) -> Vec<String> {
    std::fs::read_dir(context.temp.path().join(".cellar/uploads"))
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            name.strip_suffix(".part").is_some_and(|stem| {
                Uuid::parse_str(stem).is_ok_and(|id| format!("{id}.part") == name.as_str())
            })
        })
        .collect()
}

#[tokio::test]
async fn raw_upload_publishes_exact_bytes_and_appears_in_file_listing() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Reports").await;

    let response = context
        .app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/v1/projects/{project_id}/uploads?fileName=report.bin"
                ))
                .header("origin", EXTERNAL_ORIGIN)
                .header(ASSERTION_HEADER, VALID_ASSERTION)
                .header("content-type", "application/octet-stream")
                .body(Body::from(b"cellar-data".as_slice()))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = response_json(response).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body, json!({"name": "report.bin", "size": "11"}));
    assert_eq!(
        std::fs::read(context.project_path(project_id).join("files/report.bin")).unwrap(),
        b"cellar-data"
    );
    assert!(canonical_staging_files(&context).is_empty());

    let listed = context
        .app()
        .oneshot(
            authenticated_request("GET", &format!("/api/v1/projects/{project_id}/files"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _, body) = json_response(listed).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["name"], "report.bin");
    assert_eq!(body[0]["size"], "11");

    context.close().await;
}

#[tokio::test]
async fn upload_to_missing_project_returns_not_found_without_staging() {
    let context = TestContext::new().await;
    let response = context
        .app()
        .oneshot(upload_request(
            Uuid::now_v7(),
            "fileName=missing.bin",
            Body::from("data"),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(canonical_staging_files(&context).is_empty());
    context.close().await;
}

#[tokio::test]
async fn upload_rejects_invalid_and_noncanonical_project_ids() {
    let context = TestContext::new().await;

    for project_id in [
        "not-a-uuid",
        "0198F67E-9C0B-7000-8000-000000000001",
        "0198f67e9c0b70008000000000000001",
    ] {
        let response = context
            .app()
            .oneshot(upload_request(
                project_id,
                "fileName=report.bin",
                Body::from("data"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{project_id}");
    }

    assert!(canonical_staging_files(&context).is_empty());
    context.close().await;
}

#[tokio::test]
async fn upload_rejects_invalid_file_names() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Reports").await;

    for query in [
        "fileName=",
        "fileName=..",
        "fileName=..%2Fescape.bin",
        "fileName=CON",
    ] {
        let response = context
            .app()
            .oneshot(upload_request(project_id, query, Body::from("data")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
    }

    assert!(canonical_staging_files(&context).is_empty());
    context.close().await;
}

#[tokio::test]
async fn upload_query_requires_strict_camel_case_file_name_only() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Reports").await;

    for query in [
        "",
        "file_name=report.bin",
        "fileName=report.bin&extra=value",
        "fileName=one.bin&fileName=two.bin",
    ] {
        let response = context
            .app()
            .oneshot(upload_request(project_id, query, Body::from("data")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
    }

    assert!(canonical_staging_files(&context).is_empty());
    context.close().await;
}

#[tokio::test]
async fn upload_requires_exactly_one_octet_stream_content_type() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Reports").await;

    let missing = Request::builder()
        .method("POST")
        .uri(upload_uri(project_id, "fileName=missing.bin"))
        .header("origin", EXTERNAL_ORIGIN)
        .header(ASSERTION_HEADER, VALID_ASSERTION)
        .body(Body::from("data"))
        .unwrap();
    assert_eq!(
        context.app().oneshot(missing).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );

    for content_type in [
        "application/json",
        "Application/Octet-Stream",
        "application/octet-stream; charset=binary",
    ] {
        let request = Request::builder()
            .method("POST")
            .uri(upload_uri(project_id, "fileName=wrong.bin"))
            .header("origin", EXTERNAL_ORIGIN)
            .header(ASSERTION_HEADER, VALID_ASSERTION)
            .header("content-type", content_type)
            .body(Body::from("data"))
            .unwrap();
        assert_eq!(
            context.app().oneshot(request).await.unwrap().status(),
            StatusCode::BAD_REQUEST,
            "{content_type}"
        );
    }

    let mut duplicate = upload_request(
        project_id,
        "fileName=duplicate-header.bin",
        Body::from("data"),
    );
    duplicate.headers_mut().append(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    assert_eq!(
        context.app().oneshot(duplicate).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );

    assert!(canonical_staging_files(&context).is_empty());
    context.close().await;
}

#[tokio::test]
async fn upload_never_overwrites_an_existing_destination() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Reports").await;

    let first = context
        .app()
        .oneshot(upload_request(
            project_id,
            "fileName=same.bin",
            Body::from("original"),
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let duplicate = context
        .app()
        .oneshot(upload_request(
            project_id,
            "fileName=same.bin",
            Body::from("replacement"),
        ))
        .await
        .unwrap();
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
    assert_eq!(
        std::fs::read(context.project_path(project_id).join("files/same.bin")).unwrap(),
        b"original"
    );
    assert!(canonical_staging_files(&context).is_empty());

    context.close().await;
}

#[tokio::test]
async fn upload_rejects_empty_files_and_removes_staging() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Reports").await;
    let response = context
        .app()
        .oneshot(upload_request(
            project_id,
            "fileName=empty.bin",
            Body::empty(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        !context
            .project_path(project_id)
            .join("files/empty.bin")
            .exists()
    );
    assert!(canonical_staging_files(&context).is_empty());
    context.close().await;
}

#[tokio::test]
async fn erroring_request_body_returns_bad_request_and_removes_exact_staging_file() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Reports").await;
    let stream = futures_util::stream::iter([
        Ok::<Bytes, io::Error>(Bytes::from_static(b"partial-data")),
        Err(io::Error::other("simulated body failure")),
    ]);
    let response = context
        .app()
        .oneshot(upload_request(
            project_id,
            "fileName=broken.bin",
            Body::from_stream(stream),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        !context
            .project_path(project_id)
            .join("files/broken.bin")
            .exists()
    );
    assert!(canonical_staging_files(&context).is_empty());
    context.close().await;
}

struct CreateFailingStorage {
    failure: fn() -> StorageError,
}

impl UploadStorage for CreateFailingStorage {
    fn create_staging<'a>(&'a self, _: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { Err((self.failure)()) })
    }

    fn write_upload<'a>(&'a self, _: Uuid, _: UploadBodyReader) -> UploadStorageFuture<'a, u64> {
        Box::pin(async { panic!("write must not follow failed staging creation") })
    }

    fn sync_staging<'a>(&'a self, _: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async { panic!("sync must not follow failed staging creation") })
    }

    fn finalize_no_replace<'a>(
        &'a self,
        _: Uuid,
        _: Uuid,
        _: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, ()> {
        Box::pin(async { panic!("finalization must not follow failed staging creation") })
    }

    fn remove_staging<'a>(&'a self, _: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async {
            panic!("cleanup must not remove a staging file this request did not create")
        })
    }
}

#[tokio::test]
async fn staging_collision_is_storage_unavailable_not_destination_conflict() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Reports").await;
    let service = UploadService::new(
        Arc::new(context.database.clone()),
        Arc::new(CreateFailingStorage {
            failure: || StorageError::AlreadyExists,
        }),
    );
    let response = context
        .secure_upload(service)
        .oneshot(upload_request(
            project_id,
            "fileName=report.bin",
            Body::from("data"),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(canonical_staging_files(&context).is_empty());
    context.close().await;
}

struct PausedCollisionStorage {
    storage: Arc<cellar::storage::Storage>,
    collision_ready: Mutex<Option<tokio::sync::oneshot::Sender<Uuid>>>,
    release: Arc<tokio::sync::Semaphore>,
    returned: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    remove_called: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    remove_calls: Arc<AtomicUsize>,
}

impl UploadStorage for PausedCollisionStorage {
    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        let collision_ready = self.collision_ready.lock().unwrap().take();
        let returned = self.returned.lock().unwrap().take();
        Box::pin(async move {
            self.storage.create_staging(upload_id).await?;
            self.storage
                .write_chunk(
                    upload_id,
                    0,
                    std::io::Cursor::new(b"pre-existing-staging".to_vec()),
                )
                .await?;
            if let Some(collision_ready) = collision_ready {
                let _ = collision_ready.send(upload_id);
            }
            self.release.acquire().await.unwrap().forget();
            if let Some(returned) = returned {
                let _ = returned.send(());
            }
            Err(StorageError::AlreadyExists)
        })
    }

    fn write_upload<'a>(&'a self, _: Uuid, _: UploadBodyReader) -> UploadStorageFuture<'a, u64> {
        Box::pin(async { panic!("write must not follow staging collision") })
    }

    fn sync_staging<'a>(&'a self, _: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async { panic!("sync must not follow staging collision") })
    }

    fn finalize_no_replace<'a>(
        &'a self,
        _: Uuid,
        _: Uuid,
        _: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, ()> {
        Box::pin(async { panic!("finalization must not follow staging collision") })
    }

    fn remove_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        let remove_called = self.remove_called.lock().unwrap().take();
        Box::pin(async move {
            self.remove_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(remove_called) = remove_called {
                let _ = remove_called.send(());
            }
            self.storage.remove_staging(upload_id).await
        })
    }
}

#[tokio::test]
async fn waiter_cancellation_never_removes_unowned_staging_collision() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Reports").await;
    let (collision_tx, collision_rx) = tokio::sync::oneshot::channel();
    let (returned_tx, returned_rx) = tokio::sync::oneshot::channel();
    let (remove_tx, remove_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let remove_calls = Arc::new(AtomicUsize::new(0));
    let collision_storage = Arc::new(PausedCollisionStorage {
        storage: context.storage.clone(),
        collision_ready: Mutex::new(Some(collision_tx)),
        release: release.clone(),
        returned: Mutex::new(Some(returned_tx)),
        remove_called: Mutex::new(Some(remove_tx)),
        remove_calls: remove_calls.clone(),
    });
    let service = UploadService::new(
        Arc::new(context.database.clone()),
        collision_storage.clone(),
    );
    let request = tokio::spawn(context.secure_upload(service).oneshot(upload_request(
        project_id,
        "fileName=collision.bin",
        Body::from_stream(futures_util::stream::pending::<Result<Bytes, io::Error>>()),
    )));
    let upload_id = tokio::time::timeout(std::time::Duration::from_secs(2), collision_rx)
        .await
        .unwrap()
        .unwrap();
    let staging_path = context
        .temp
        .path()
        .join(".cellar/uploads")
        .join(format!("{upload_id}.part"));
    assert_eq!(
        std::fs::read(&staging_path).unwrap(),
        b"pre-existing-staging"
    );

    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    release.add_permits(1);
    returned_rx.await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), remove_rx)
            .await
            .is_err(),
        "waiter cleanup attempted to remove staging it never owned"
    );
    assert_eq!(remove_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read(staging_path).unwrap(),
        b"pre-existing-staging"
    );
    drop(collision_storage);
    context.close().await;
}

#[tokio::test]
async fn staging_capacity_failure_returns_insufficient_storage() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Reports").await;
    let service = UploadService::new(
        Arc::new(context.database.clone()),
        Arc::new(CreateFailingStorage {
            failure: || StorageError::InsufficientSpace,
        }),
    );
    let response = context
        .secure_upload(service)
        .oneshot(upload_request(
            project_id,
            "fileName=large.bin",
            Body::from("data"),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::INSUFFICIENT_STORAGE);
    assert!(canonical_staging_files(&context).is_empty());
    context.close().await;
}

#[tokio::test]
async fn internal_storage_failure_returns_safe_service_unavailable() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Reports").await;
    let service = UploadService::new(
        Arc::new(context.database.clone()),
        Arc::new(CreateFailingStorage {
            failure: || StorageError::Io {
                source: io::Error::other(r"D:\private\cellar\upload.part"),
            },
        }),
    );
    let response = context
        .secure_upload(service)
        .oneshot(upload_request(
            project_id,
            "fileName=private.bin",
            Body::from("data"),
        ))
        .await
        .unwrap();
    let (status, body) = response_json(response).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!body.to_string().contains("private\\cellar"));
    assert!(!body.to_string().contains("upload.part"));
    assert!(canonical_staging_files(&context).is_empty());
    context.close().await;
}

struct FailingRepository;

impl UploadRepository for FailingRepository {
    fn get_project<'a>(&'a self, _: Uuid) -> UploadFuture<'a, Result<Option<ProjectRow>, DbError>> {
        Box::pin(async { Err(DbError::CorruptData) })
    }
}

#[tokio::test]
async fn database_failure_returns_safe_service_unavailable_before_staging() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Reports").await;
    let service = UploadService::new(Arc::new(FailingRepository), context.storage.clone());
    let response = context
        .secure_upload(service)
        .oneshot(upload_request(
            project_id,
            "fileName=report.bin",
            Body::from("data"),
        ))
        .await
        .unwrap();
    let (status, body) = response_json(response).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "upload_unavailable");
    assert!(canonical_staging_files(&context).is_empty());
    context.close().await;
}

struct CreatedThenPendingStorage {
    storage: Arc<cellar::storage::Storage>,
    created: Mutex<Option<tokio::sync::oneshot::Sender<Uuid>>>,
    continued: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Arc<tokio::sync::Semaphore>,
}

impl UploadStorage for CreatedThenPendingStorage {
    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        let created = self.created.lock().unwrap().take();
        let continued = self.continued.lock().unwrap().take();
        Box::pin(async move {
            self.storage.create_staging(upload_id).await?;
            if let Some(created) = created {
                let _ = created.send(upload_id);
            }
            self.release.acquire().await.unwrap().forget();
            if let Some(continued) = continued {
                let _ = continued.send(());
            }
            Ok(())
        })
    }

    fn write_upload<'a>(
        &'a self,
        upload_id: Uuid,
        reader: UploadBodyReader,
    ) -> UploadStorageFuture<'a, u64> {
        Box::pin(async move { self.storage.write_chunk(upload_id, 0, reader).await })
    }

    fn sync_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.sync_staging(upload_id).await })
    }

    fn finalize_no_replace<'a>(
        &'a self,
        upload_id: Uuid,
        project_id: Uuid,
        name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move {
            self.storage
                .finalize_no_replace(upload_id, project_id, name)
                .await
        })
    }

    fn remove_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.remove_staging(upload_id).await })
    }
}

#[tokio::test]
async fn cancellation_during_staging_creation_is_supervised_until_ordered_cleanup() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Reports").await;
    let (created_tx, created_rx) = tokio::sync::oneshot::channel();
    let (continued_tx, continued_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let service = UploadService::new(
        Arc::new(context.database.clone()),
        Arc::new(CreatedThenPendingStorage {
            storage: context.storage.clone(),
            created: Mutex::new(Some(created_tx)),
            continued: Mutex::new(Some(continued_tx)),
            release: release.clone(),
        }),
    );
    let body = Body::from_stream(futures_util::stream::pending::<Result<Bytes, io::Error>>());
    let request = tokio::spawn(context.secure_upload(service).oneshot(upload_request(
        project_id,
        "fileName=cancelled.bin",
        body,
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
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0),
        "handler cancellation must not race cleanup ahead of paused creation"
    );
    release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(2), continued_rx)
        .await
        .expect("server-owned staging creation was cancelled with its waiter")
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while context
            .storage
            .staging_len(upload_id)
            .await
            .unwrap()
            .is_some()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled upload did not remove its exact staging file");
    assert!(
        !context
            .project_path(project_id)
            .join("files/cancelled.bin")
            .exists()
    );
    assert!(canonical_staging_files(&context).is_empty());
    context.close().await;
}

struct WriteObservingStorage {
    storage: Arc<cellar::storage::Storage>,
    started: Mutex<Option<tokio::sync::oneshot::Sender<Uuid>>>,
    continued: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Arc<tokio::sync::Semaphore>,
}

impl UploadStorage for WriteObservingStorage {
    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.create_staging(upload_id).await })
    }

    fn write_upload<'a>(
        &'a self,
        upload_id: Uuid,
        reader: UploadBodyReader,
    ) -> UploadStorageFuture<'a, u64> {
        let started = self.started.lock().unwrap().take();
        let continued = self.continued.lock().unwrap().take();
        Box::pin(async move {
            if let Some(started) = started {
                let _ = started.send(upload_id);
            }
            self.release.acquire().await.unwrap().forget();
            if let Some(continued) = continued {
                let _ = continued.send(());
            }
            self.storage.write_chunk(upload_id, 0, reader).await
        })
    }

    fn sync_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.sync_staging(upload_id).await })
    }

    fn finalize_no_replace<'a>(
        &'a self,
        upload_id: Uuid,
        project_id: Uuid,
        name: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move {
            self.storage
                .finalize_no_replace(upload_id, project_id, name)
                .await
        })
    }

    fn remove_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.remove_staging(upload_id).await })
    }
}

#[tokio::test]
async fn cancellation_during_write_is_supervised_until_body_failure_cleanup() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Reports").await;
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (continued_tx, continued_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let service = UploadService::new(
        Arc::new(context.database.clone()),
        Arc::new(WriteObservingStorage {
            storage: context.storage.clone(),
            started: Mutex::new(Some(started_tx)),
            continued: Mutex::new(Some(continued_tx)),
            release: release.clone(),
        }),
    );
    let stream = futures_util::stream::pending::<Result<Bytes, io::Error>>();
    let request = tokio::spawn(context.secure_upload(service).oneshot(upload_request(
        project_id,
        "fileName=write-cancelled.bin",
        Body::from_stream(stream),
    )));
    let upload_id = tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0)
    );

    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(0)
    );
    release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(2), continued_rx)
        .await
        .expect("server-owned body write was cancelled with its waiter")
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while context
            .storage
            .staging_len(upload_id)
            .await
            .unwrap()
            .is_some()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("supervised failed write did not clean staging");
    assert!(
        !context
            .project_path(project_id)
            .join("files/write-cancelled.bin")
            .exists()
    );
    context.close().await;
}

struct CleanupFailingStorage {
    storage: Arc<cellar::storage::Storage>,
}

impl UploadStorage for CleanupFailingStorage {
    fn create_staging<'a>(&'a self, upload_id: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async move { self.storage.create_staging(upload_id).await })
    }

    fn write_upload<'a>(&'a self, _: Uuid, _: UploadBodyReader) -> UploadStorageFuture<'a, u64> {
        Box::pin(async { Err(StorageError::InvalidBody) })
    }

    fn sync_staging<'a>(&'a self, _: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async { panic!("sync must not follow failed body") })
    }

    fn finalize_no_replace<'a>(
        &'a self,
        _: Uuid,
        _: Uuid,
        _: &'a SafeFileName,
    ) -> UploadStorageFuture<'a, ()> {
        Box::pin(async { panic!("finalization must not follow failed body") })
    }

    fn remove_staging<'a>(&'a self, _: Uuid) -> UploadStorageFuture<'a, ()> {
        Box::pin(async {
            Err(StorageError::Io {
                source: io::Error::other(r"D:\private\cleanup\upload.part"),
            })
        })
    }
}

#[tokio::test]
async fn body_failure_with_failed_cleanup_returns_safe_service_unavailable() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Reports").await;
    let service = UploadService::new(
        Arc::new(context.database.clone()),
        Arc::new(CleanupFailingStorage {
            storage: context.storage.clone(),
        }),
    );
    let response = context
        .secure_upload(service)
        .oneshot(upload_request(
            project_id,
            "fileName=cleanup-failure.bin",
            Body::from("data"),
        ))
        .await
        .unwrap();
    let (status, body) = response_json(response).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "upload_unavailable");
    assert!(!body.to_string().contains("private"));
    assert!(!body.to_string().contains("upload.part"));
    context.close().await;
}

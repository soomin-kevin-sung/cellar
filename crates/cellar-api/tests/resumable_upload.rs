use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderName, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use cellar_api::routes::session::session_router_with_routes;
use cellar_api::routes::uploads::{
    MAX_UPLOAD_CHUNK_BYTES, uploads_router_with_clock, uploads_router_with_clock_and_idle_timeout,
};
use cellar_auth::{
    AccessClaims, CompareAndSet, CsrfManager, EnrollmentService, EnrollmentSnapshot,
    EnrollmentStore, EnrollmentStoreError,
};
use cellar_core::{
    ProjectId, UploadId, UploadLimits, UploadService, UploadStagingError, UploadStagingStore,
};
use cellar_db::{FilenameCollation, SqliteUploadRepository, migrate, open_pool};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use sqlx::SqlitePool;
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt as _;

const NOW: i64 = 50_000;
const ORIGIN: &str = "https://cellar.example";
const SUBJECT: &str = "owner-subject";

struct EnrolledStore;

impl EnrollmentStore for EnrolledStore {
    fn load(&self) -> Result<EnrollmentSnapshot, EnrollmentStoreError> {
        Ok(EnrollmentSnapshot::enrolled(ORIGIN, SUBJECT))
    }

    fn compare_and_set_owner(
        &self,
        _: &EnrollmentSnapshot,
        _: &str,
    ) -> Result<CompareAndSet, EnrollmentStoreError> {
        Ok(CompareAndSet::Changed)
    }
}

fn claims() -> AccessClaims {
    AccessClaims {
        iss: "https://issuer.example".into(),
        aud: vec!["aud".into()],
        sub: SUBJECT.into(),
        email: Some("owner@example.com".into()),
        exp: NOW + 40_000,
        nbf: NOW,
        iat: NOW,
        r#type: "app".into(),
    }
}

#[derive(Default)]
struct MemoryStaging {
    files: Mutex<HashMap<UploadId, Vec<u8>>>,
    available: AtomicI64,
    short_write: AtomicBool,
    fail_create: AtomicBool,
    block_write: AtomicBool,
    write_started: tokio::sync::Notify,
    release_write: tokio::sync::Notify,
}

impl MemoryStaging {
    fn with_space(bytes: i64) -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            available: AtomicI64::new(bytes),
            short_write: AtomicBool::new(false),
            fail_create: AtomicBool::new(false),
            block_write: AtomicBool::new(false),
            write_started: tokio::sync::Notify::new(),
            release_write: tokio::sync::Notify::new(),
        }
    }

    fn replace(&self, id: UploadId, bytes: &[u8]) {
        self.files.lock().unwrap().insert(id, bytes.to_vec());
    }

    fn bytes(&self, id: UploadId) -> Vec<u8> {
        self.files.lock().unwrap()[&id].clone()
    }
}

#[async_trait]
impl UploadStagingStore for MemoryStaging {
    async fn create(&self, id: UploadId) -> Result<(), UploadStagingError> {
        if self.fail_create.swap(false, Ordering::SeqCst) {
            return Err(UploadStagingError::Unavailable);
        }
        let mut files = self.files.lock().unwrap();
        if files.insert(id, Vec::new()).is_some() {
            return Err(UploadStagingError::Unavailable);
        }
        Ok(())
    }

    async fn length(&self, id: UploadId) -> Result<i64, UploadStagingError> {
        self.files
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|bytes| i64::try_from(bytes.len()).ok())
            .ok_or(UploadStagingError::NotFound)
    }

    async fn read_exact(
        &self,
        id: UploadId,
        offset: i64,
        length: i64,
    ) -> Result<Vec<u8>, UploadStagingError> {
        let start = usize::try_from(offset).map_err(|_| UploadStagingError::Unavailable)?;
        let length = usize::try_from(length).map_err(|_| UploadStagingError::Unavailable)?;
        let end = start
            .checked_add(length)
            .ok_or(UploadStagingError::Unavailable)?;
        self.files
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|bytes| bytes.get(start..end))
            .map(<[u8]>::to_vec)
            .ok_or(UploadStagingError::Unavailable)
    }

    async fn truncate(&self, id: UploadId, length: i64) -> Result<(), UploadStagingError> {
        let length = usize::try_from(length).map_err(|_| UploadStagingError::Unavailable)?;
        let mut files = self.files.lock().unwrap();
        let bytes = files.get_mut(&id).ok_or(UploadStagingError::NotFound)?;
        bytes.truncate(length);
        Ok(())
    }

    async fn write_exact_and_flush(
        &self,
        id: UploadId,
        offset: i64,
        input: &[u8],
    ) -> Result<(), UploadStagingError> {
        let offset = usize::try_from(offset).map_err(|_| UploadStagingError::Unavailable)?;
        if self.block_write.load(Ordering::SeqCst) {
            self.write_started.notify_one();
            self.release_write.notified().await;
        }
        let mut files = self.files.lock().unwrap();
        let bytes = files.get_mut(&id).ok_or(UploadStagingError::NotFound)?;
        if bytes.len() != offset {
            return Err(UploadStagingError::Unavailable);
        }
        let written = if self.short_write.swap(false, Ordering::SeqCst) {
            input.len().saturating_sub(1)
        } else {
            input.len()
        };
        bytes.extend_from_slice(&input[..written]);
        Ok(())
    }

    async fn remove(&self, id: UploadId) -> Result<(), UploadStagingError> {
        self.files
            .lock()
            .unwrap()
            .remove(&id)
            .map(|_| ())
            .ok_or(UploadStagingError::NotFound)
    }

    async fn available_space(&self) -> Result<i64, UploadStagingError> {
        Ok(self.available.load(Ordering::SeqCst))
    }
}

struct Harness {
    app: axum::Router,
    _directory: TempDir,
    pool: SqlitePool,
    staging: Arc<MemoryStaging>,
    project_id: ProjectId,
}

fn default_limits() -> UploadLimits {
    UploadLimits {
        max_chunk_size: 8,
        max_active_sessions: 8,
        max_concurrent_uploads: 3,
        free_space_reserve: 10,
        session_ttl: Duration::days(7),
    }
}

async fn make_harness(limits: UploadLimits, available: i64) -> Harness {
    make_harness_with_idle_timeout(limits, available, None).await
}

async fn make_harness_with_idle_timeout(
    limits: UploadLimits,
    available: i64,
    idle_timeout: Option<std::time::Duration>,
) -> Harness {
    let directory = TempDir::new().unwrap();
    let collation = FilenameCollation::windows_ordinal_ci_v1(|left, right| left.cmp(right));
    let pool = open_pool(directory.path().join("cellar.db"), collation)
        .await
        .unwrap();
    migrate(&pool).await.unwrap();
    let project_id = ProjectId::new();
    sqlx::query(
        "INSERT INTO project
         (id, name, description, status, version, created_at, updated_at)
         VALUES (?, 'uploads', '', 'active', 1,
                 '1970-01-01T00:00:00.000000000Z',
                 '1970-01-01T00:00:00.000000000Z')",
    )
    .bind(project_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    let staging = Arc::new(MemoryStaging::with_space(available));
    let repository = Arc::new(SqliteUploadRepository::new(pool.clone()));
    let service = UploadService::new(repository, staging.clone(), limits);
    let protected = if let Some(idle_timeout) = idle_timeout {
        uploads_router_with_clock_and_idle_timeout::<EnrolledStore, _>(
            service,
            || OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
            idle_timeout,
        )
    } else {
        uploads_router_with_clock::<EnrolledStore, _>(service, || {
            OffsetDateTime::from_unix_timestamp(NOW).unwrap()
        })
    };
    let app = session_router_with_routes(
        EnrollmentService::new(Arc::new(EnrolledStore)),
        Arc::new(CsrfManager::new()),
        || NOW,
        protected,
    );
    Harness {
        app,
        _directory: directory,
        pool,
        staging,
        project_id,
    }
}

fn request(method: &str, uri: &str, body: Body) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .body(body)
        .unwrap();
    request.extensions_mut().insert(claims());
    request
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

async fn csrf_token(app: &axum::Router) -> String {
    let response = app
        .clone()
        .oneshot(request("GET", "/api/v1/session", Body::empty()))
        .await
        .unwrap();
    response_json(response).await["csrf_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn authorize_mutation(request: &mut Request<Body>, token: &str) {
    request
        .headers_mut()
        .insert(header::ORIGIN, ORIGIN.parse().unwrap());
    request.headers_mut().insert(
        HeaderName::from_static("x-cellar-csrf"),
        token.parse().unwrap(),
    );
}

async fn create_upload(
    harness: &Harness,
    token: &str,
    name: &str,
    expected_size: &str,
) -> axum::response::Response {
    let mut request = request(
        "POST",
        "/api/v1/uploads",
        Body::from(
            json!({
                "projectId": harness.project_id.to_string(),
                "destinationName": name,
                "expectedSize": expected_size
            })
            .to_string(),
        ),
    );
    request
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    authorize_mutation(&mut request, token);
    harness.app.clone().oneshot(request).await.unwrap()
}

async fn created_id(harness: &Harness, token: &str, name: &str, size: &str) -> UploadId {
    let response = create_upload(harness, token, name, size).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    response_json(response).await["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

async fn chunk(
    harness: &Harness,
    token: &str,
    id: UploadId,
    offset: &str,
    bytes: &[u8],
) -> axum::response::Response {
    harness
        .app
        .clone()
        .oneshot(chunk_request(token, id, offset, bytes))
        .await
        .unwrap()
}

fn chunk_request(token: &str, id: UploadId, offset: &str, bytes: &[u8]) -> Request<Body> {
    let mut request = request(
        "PUT",
        &format!("/api/v1/uploads/{id}/chunk"),
        Body::from(bytes.to_vec()),
    );
    request.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    request.headers_mut().insert(
        header::CONTENT_LENGTH,
        bytes.len().to_string().parse().unwrap(),
    );
    request.headers_mut().insert(
        HeaderName::from_static("upload-offset"),
        offset.parse().unwrap(),
    );
    let digest = STANDARD.encode(Sha256::digest(bytes));
    request.headers_mut().insert(
        HeaderName::from_static("digest"),
        format!("sha-256={digest}").parse().unwrap(),
    );
    authorize_mutation(&mut request, token);
    request
}

async fn status(harness: &Harness, id: UploadId) -> axum::response::Response {
    harness
        .app
        .clone()
        .oneshot(request(
            "GET",
            &format!("/api/v1/uploads/{id}"),
            Body::empty(),
        ))
        .await
        .unwrap()
}

async fn assert_error(response: axum::response::Response, status: StatusCode, code: &str) {
    assert_eq!(response.status(), status);
    let body = response_json(response).await;
    assert_eq!(body["code"], code);
    assert_eq!(body["details"], json!({}));
    assert!(body["requestId"].as_str().is_some_and(|id| !id.is_empty()));
}

async fn assert_error_with_request_id(
    response: axum::response::Response,
    status: StatusCode,
    code: &str,
    expected_request_id: &str,
) {
    assert_eq!(response.status(), status);
    assert_eq!(response.headers()["x-request-id"], expected_request_id);
    let body = response_json(response).await;
    assert_eq!(body["code"], code);
    assert_eq!(body["requestId"], expected_request_id);
    assert_eq!(body["details"], json!({}));
}

#[test]
fn upload_protocol_advertises_the_required_chunk_limit() {
    assert_eq!(MAX_UPLOAD_CHUNK_BYTES, 32 * 1024 * 1024);
}

#[tokio::test]
async fn configured_chunk_limit_cannot_exceed_the_hard_protocol_maximum() {
    let mut limits = default_limits();
    limits.max_chunk_size = MAX_UPLOAD_CHUNK_BYTES + 1;
    let harness = make_harness(limits, 1_000).await;
    let token = csrf_token(&harness.app).await;
    let response = create_upload(&harness, &token, "bounded.bin", "0").await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response_json(response).await["maxChunkSize"],
        MAX_UPLOAD_CHUNK_BYTES.to_string()
    );
    harness.pool.close().await;
}

#[tokio::test]
async fn resumes_from_committed_offset_and_handles_retries_and_final_short_chunks() {
    let harness = make_harness(default_limits(), 1_000).await;
    let token = csrf_token(&harness.app).await;
    let id = created_id(&harness, &token, "resume.bin", "5").await;

    let first = chunk(&harness, &token, id, "0", b"abc").await;
    assert_eq!(first.status(), StatusCode::NO_CONTENT);
    assert_eq!(first.headers()["upload-offset"], "3");
    let current = response_json(status(&harness, id).await).await;
    assert_eq!(current["expectedSize"], "5");
    assert_eq!(current["committedOffset"], "3");
    assert_eq!(current["maxChunkSize"], "8");

    let retry = chunk(&harness, &token, id, "0", b"abc").await;
    assert_eq!(retry.status(), StatusCode::NO_CONTENT);
    assert_eq!(retry.headers()["upload-offset"], "3");
    assert_error(
        chunk(&harness, &token, id, "0", b"xyz").await,
        StatusCode::CONFLICT,
        "upload_conflict",
    )
    .await;
    assert_error(
        chunk(&harness, &token, id, "2", b"zz").await,
        StatusCode::CONFLICT,
        "upload_conflict",
    )
    .await;
    assert_error(
        chunk(&harness, &token, id, "4", b"z").await,
        StatusCode::CONFLICT,
        "upload_conflict",
    )
    .await;

    let final_chunk = chunk(&harness, &token, id, "3", b"de").await;
    assert_eq!(final_chunk.status(), StatusCode::NO_CONTENT);
    assert_eq!(final_chunk.headers()["upload-offset"], "5");
    assert_eq!(harness.staging.bytes(id), b"abcde");

    let zero = created_id(&harness, &token, "empty.bin", "0").await;
    assert_eq!(
        response_json(status(&harness, zero).await).await["committedOffset"],
        "0"
    );
    harness.pool.close().await;
}

#[tokio::test]
async fn recovers_matching_pending_chunks_truncates_crash_tails_and_fails_short_durable_data() {
    let harness = make_harness(default_limits(), 1_000).await;
    let token = csrf_token(&harness.app).await;
    let id = created_id(&harness, &token, "recover.bin", "8").await;
    let digest = Sha256::digest(b"abc").to_vec();
    sqlx::query(
        "UPDATE upload_session SET state = 'uploading', pending_offset = 0,
         pending_length = 3, pending_digest = ? WHERE id = ?",
    )
    .bind(digest)
    .bind(id.to_string())
    .execute(&harness.pool)
    .await
    .unwrap();
    harness.staging.replace(id, b"abc");
    assert_eq!(
        response_json(status(&harness, id).await).await["committedOffset"],
        "3"
    );

    harness.staging.replace(id, b"abcTAIL");
    assert_eq!(
        response_json(status(&harness, id).await).await["committedOffset"],
        "3"
    );
    assert_eq!(harness.staging.bytes(id), b"abc");

    sqlx::query("UPDATE upload_session SET committed_offset = 4 WHERE id = ?")
        .bind(id.to_string())
        .execute(&harness.pool)
        .await
        .unwrap();
    assert_error(
        status(&harness, id).await,
        StatusCode::SERVICE_UNAVAILABLE,
        "upload_unavailable",
    )
    .await;
    let state: String = sqlx::query_scalar("SELECT state FROM upload_session WHERE id = ?")
        .bind(id.to_string())
        .fetch_one(&harness.pool)
        .await
        .unwrap();
    assert_eq!(state, "failed");
    harness.pool.close().await;
}

#[tokio::test]
async fn short_durable_write_fails_closed_and_cancel_is_idempotent() {
    let harness = make_harness(default_limits(), 1_000).await;
    let token = csrf_token(&harness.app).await;
    let id = created_id(&harness, &token, "short.bin", "3").await;
    harness.staging.short_write.store(true, Ordering::SeqCst);
    assert_error(
        chunk(&harness, &token, id, "0", b"abc").await,
        StatusCode::SERVICE_UNAVAILABLE,
        "upload_unavailable",
    )
    .await;

    let cancel_id = created_id(&harness, &token, "cancel.bin", "3").await;
    let mut cancel = request(
        "DELETE",
        &format!("/api/v1/uploads/{cancel_id}"),
        Body::empty(),
    );
    authorize_mutation(&mut cancel, &token);
    assert_eq!(
        harness.app.clone().oneshot(cancel).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    let mut repeat = request(
        "DELETE",
        &format!("/api/v1/uploads/{cancel_id}"),
        Body::empty(),
    );
    authorize_mutation(&mut repeat, &token);
    assert_eq!(
        harness.app.clone().oneshot(repeat).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_error(
        status(&harness, cancel_id).await,
        StatusCode::NOT_FOUND,
        "upload_not_found",
    )
    .await;
    harness.pool.close().await;
}

#[tokio::test]
async fn enforces_expiry_chunk_size_session_concurrency_and_free_space_limits() {
    let mut limits = default_limits();
    limits.max_chunk_size = 4;
    limits.max_active_sessions = 2;
    limits.max_concurrent_uploads = 1;
    let harness = make_harness(limits, 1_000).await;
    let token = csrf_token(&harness.app).await;
    let first = created_id(&harness, &token, "first.bin", "8").await;
    let second = created_id(&harness, &token, "second.bin", "8").await;
    assert_error(
        create_upload(&harness, &token, "third.bin", "1").await,
        StatusCode::TOO_MANY_REQUESTS,
        "upload_capacity_exceeded",
    )
    .await;
    assert_error(
        chunk(&harness, &token, first, "0", b"12345").await,
        StatusCode::PAYLOAD_TOO_LARGE,
        "upload_chunk_too_large",
    )
    .await;
    assert_eq!(
        chunk(&harness, &token, first, "0", b"1234").await.status(),
        StatusCode::NO_CONTENT
    );
    let concurrency_response = chunk(&harness, &token, second, "0", b"12").await;
    assert_eq!(concurrency_response.headers()[header::RETRY_AFTER], "5");
    assert_error(
        concurrency_response,
        StatusCode::TOO_MANY_REQUESTS,
        "upload_capacity_exceeded",
    )
    .await;

    sqlx::query(
        "UPDATE upload_session SET expires_at = '1970-01-01T00:00:00.000000000Z' WHERE id = ?",
    )
    .bind(second.to_string())
    .execute(&harness.pool)
    .await
    .unwrap();
    assert_error(
        status(&harness, second).await,
        StatusCode::GONE,
        "upload_expired",
    )
    .await;
    harness.pool.close().await;

    let low_space = make_harness(default_limits(), 15).await;
    let token = csrf_token(&low_space.app).await;
    assert_error(
        create_upload(&low_space, &token, "large.bin", "6").await,
        StatusCode::INSUFFICIENT_STORAGE,
        "insufficient_storage",
    )
    .await;
    low_space.pool.close().await;

    let write_space = make_harness(default_limits(), 100).await;
    let token = csrf_token(&write_space.app).await;
    let id = created_id(&write_space, &token, "write-space.bin", "3").await;
    write_space.staging.available.store(12, Ordering::SeqCst);
    assert_error(
        chunk(&write_space, &token, id, "0", b"abc").await,
        StatusCode::INSUFFICIENT_STORAGE,
        "insufficient_storage",
    )
    .await;
    write_space.pool.close().await;
}

#[tokio::test]
async fn requires_canonical_decimal_headers_exact_lengths_and_rfc_sha256_digest() {
    let harness = make_harness(default_limits(), 1_000).await;
    let token = csrf_token(&harness.app).await;
    assert_error(
        create_upload(&harness, &token, "bad.bin", "01").await,
        StatusCode::BAD_REQUEST,
        "invalid_expected_size",
    )
    .await;
    let id = created_id(&harness, &token, "headers.bin", "3").await;
    let mut bad = request(
        "PUT",
        &format!("/api/v1/uploads/{id}/chunk"),
        Body::from("abc"),
    );
    bad.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    bad.headers_mut()
        .insert(header::CONTENT_LENGTH, "2".parse().unwrap());
    bad.headers_mut().insert(
        HeaderName::from_static("upload-offset"),
        "0".parse().unwrap(),
    );
    bad.headers_mut().insert(
        HeaderName::from_static("digest"),
        "sha-256=invalid".parse().unwrap(),
    );
    authorize_mutation(&mut bad, &token);
    assert_error(
        harness.app.clone().oneshot(bad).await.unwrap(),
        StatusCode::BAD_REQUEST,
        "invalid_digest",
    )
    .await;

    let digest = STANDARD.encode(Sha256::digest(b"abc"));
    let mut wrong_length = request(
        "PUT",
        &format!("/api/v1/uploads/{id}/chunk"),
        Body::from("abc"),
    );
    wrong_length.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    wrong_length
        .headers_mut()
        .insert(header::CONTENT_LENGTH, "2".parse().unwrap());
    wrong_length.headers_mut().insert(
        HeaderName::from_static("upload-offset"),
        "0".parse().unwrap(),
    );
    wrong_length.headers_mut().insert(
        HeaderName::from_static("digest"),
        format!("sha-256={digest}").parse().unwrap(),
    );
    authorize_mutation(&mut wrong_length, &token);
    assert_error(
        harness.app.clone().oneshot(wrong_length).await.unwrap(),
        StatusCode::BAD_REQUEST,
        "content_length_mismatch",
    )
    .await;

    let mut tiny_limits = default_limits();
    tiny_limits.max_chunk_size = 2;
    let tiny = make_harness(tiny_limits, 1_000).await;
    let tiny_token = csrf_token(&tiny.app).await;
    let tiny_id = created_id(&tiny, &tiny_token, "raw-limit.bin", "3").await;
    let digest = STANDARD.encode(Sha256::digest(b"abc"));
    let mut forged = request(
        "PUT",
        &format!("/api/v1/uploads/{tiny_id}/chunk"),
        Body::from("abc"),
    );
    forged.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    forged
        .headers_mut()
        .insert(header::CONTENT_LENGTH, "2".parse().unwrap());
    forged.headers_mut().insert(
        HeaderName::from_static("upload-offset"),
        "0".parse().unwrap(),
    );
    forged.headers_mut().insert(
        HeaderName::from_static("digest"),
        format!("sha-256={digest}").parse().unwrap(),
    );
    authorize_mutation(&mut forged, &tiny_token);
    assert_error(
        tiny.app.clone().oneshot(forged).await.unwrap(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "upload_chunk_too_large",
    )
    .await;
    tiny.pool.close().await;
    harness.pool.close().await;
}

#[tokio::test]
async fn creation_requires_canonical_project_and_parent_uuid_text() {
    let harness = make_harness(default_limits(), 1_000).await;
    let token = csrf_token(&harness.app).await;
    let parent_id = cellar_core::FileEntryId::new();
    sqlx::query(
        "INSERT INTO file_entry
         (id, project_id, exact_name, kind, platform_kind, size,
          mtime_filetime_100ns, hash_state, state, revision, scan_generation, observed_at)
         VALUES (?, ?, 'folder', 'directory', 'directory', 0, 0,
                 'unknown', 'live', 1, 1, '1970-01-01T00:00:00.000000000Z')",
    )
    .bind(parent_id.to_string())
    .bind(harness.project_id.to_string())
    .execute(&harness.pool)
    .await
    .unwrap();

    for (project_id, parent, code) in [
        (
            harness.project_id.to_string().to_uppercase(),
            None,
            "invalid_project_id",
        ),
        (
            harness.project_id.to_string().replace('-', ""),
            None,
            "invalid_project_id",
        ),
        (
            harness.project_id.to_string(),
            Some(parent_id.to_string().to_uppercase()),
            "invalid_destination_parent_id",
        ),
        (
            harness.project_id.to_string(),
            Some(parent_id.to_string().replace('-', "")),
            "invalid_destination_parent_id",
        ),
    ] {
        let mut request = request(
            "POST",
            "/api/v1/uploads",
            Body::from(
                json!({
                    "projectId": project_id,
                    "destinationParentId": parent,
                    "destinationName": format!("{code}.bin"),
                    "expectedSize": "0"
                })
                .to_string(),
            ),
        );
        request
            .headers_mut()
            .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        authorize_mutation(&mut request, &token);
        assert_error(
            harness.app.clone().oneshot(request).await.unwrap(),
            StatusCode::BAD_REQUEST,
            code,
        )
        .await;
    }
    harness.pool.close().await;
}

#[tokio::test]
async fn expired_sessions_release_destination_session_concurrency_and_reserved_space() {
    let mut limits = default_limits();
    limits.max_active_sessions = 1;
    let session_capacity = make_harness(limits, 100).await;
    let token = csrf_token(&session_capacity.app).await;
    let expired = created_id(&session_capacity, &token, "same.bin", "80").await;
    sqlx::query(
        "UPDATE upload_session SET expires_at = '1970-01-01T00:00:00.000000000Z' WHERE id = ?",
    )
    .bind(expired.to_string())
    .execute(&session_capacity.pool)
    .await
    .unwrap();
    let replacement = create_upload(&session_capacity, &token, "same.bin", "80").await;
    assert_eq!(replacement.status(), StatusCode::CREATED);
    let state: String = sqlx::query_scalar("SELECT state FROM upload_session WHERE id = ?")
        .bind(expired.to_string())
        .fetch_one(&session_capacity.pool)
        .await
        .unwrap();
    assert_eq!(state, "failed");
    assert!(
        !session_capacity
            .staging
            .files
            .lock()
            .unwrap()
            .contains_key(&expired)
    );
    session_capacity.pool.close().await;

    let mut limits = default_limits();
    limits.max_concurrent_uploads = 1;
    let concurrency = make_harness(limits, 1_000).await;
    let token = csrf_token(&concurrency.app).await;
    let expired = created_id(&concurrency, &token, "expired-writer.bin", "3").await;
    assert_eq!(
        chunk(&concurrency, &token, expired, "0", b"a")
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    sqlx::query(
        "UPDATE upload_session SET expires_at = '1970-01-01T00:00:00.000000000Z' WHERE id = ?",
    )
    .bind(expired.to_string())
    .execute(&concurrency.pool)
    .await
    .unwrap();
    let next = created_id(&concurrency, &token, "next.bin", "1").await;
    assert_eq!(
        chunk(&concurrency, &token, next, "0", b"z").await.status(),
        StatusCode::NO_CONTENT
    );
    concurrency.pool.close().await;
}

#[tokio::test]
async fn every_upload_capacity_response_has_bounded_retry_after() {
    let mut limits = default_limits();
    limits.max_active_sessions = 1;
    let harness = make_harness(limits, 1_000).await;
    let token = csrf_token(&harness.app).await;
    created_id(&harness, &token, "one.bin", "1").await;
    let response = create_upload(&harness, &token, "two.bin", "1").await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after: u64 = response.headers()[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry_after));
    harness.pool.close().await;
}

#[tokio::test]
async fn upload_security_and_method_errors_share_request_id_envelopes() {
    let harness = make_harness(default_limits(), 1_000).await;
    let unauthenticated_id = "upload-unauthenticated";
    let mut unauthenticated = Request::builder()
        .uri("/api/v1/uploads")
        .body(Body::empty())
        .unwrap();
    unauthenticated.headers_mut().insert(
        HeaderName::from_static("x-request-id"),
        unauthenticated_id.parse().unwrap(),
    );
    assert_error_with_request_id(
        harness.app.clone().oneshot(unauthenticated).await.unwrap(),
        StatusCode::UNAUTHORIZED,
        "missing_authentication",
        unauthenticated_id,
    )
    .await;

    let forbidden_id = "upload-forbidden";
    let mut wrong_owner = claims();
    wrong_owner.sub = "different-owner".into();
    let mut forbidden = Request::builder()
        .uri("/api/v1/uploads")
        .body(Body::empty())
        .unwrap();
    forbidden.extensions_mut().insert(wrong_owner);
    forbidden.headers_mut().insert(
        HeaderName::from_static("x-request-id"),
        forbidden_id.parse().unwrap(),
    );
    assert_error_with_request_id(
        harness.app.clone().oneshot(forbidden).await.unwrap(),
        StatusCode::FORBIDDEN,
        "claim_forbidden",
        forbidden_id,
    )
    .await;

    let token = csrf_token(&harness.app).await;
    let id = created_id(&harness, &token, "method.bin", "0").await;
    let method_id = "upload-method";
    let mut unsupported = request("POST", &format!("/api/v1/uploads/{id}"), Body::empty());
    unsupported.headers_mut().insert(
        HeaderName::from_static("x-request-id"),
        method_id.parse().unwrap(),
    );
    authorize_mutation(&mut unsupported, &token);
    assert_error_with_request_id(
        harness.app.clone().oneshot(unsupported).await.unwrap(),
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        method_id,
    )
    .await;
    harness.pool.close().await;
}

#[tokio::test]
async fn live_chunk_lease_blocks_status_duplicate_put_and_cancel_until_cas_commit() {
    let harness = make_harness(default_limits(), 1_000).await;
    let token = csrf_token(&harness.app).await;

    let status_id = created_id(&harness, &token, "blocked-status.bin", "3").await;
    harness.staging.block_write.store(true, Ordering::SeqCst);
    let app = harness.app.clone();
    let put_request = chunk_request(&token, status_id, "0", b"abc");
    let writer = tokio::spawn(async move { app.oneshot(put_request).await.unwrap() });
    harness.staging.write_started.notified().await;
    let pending: (i64, i64, Vec<u8>) = sqlx::query_as(
        "SELECT pending_offset, pending_length, pending_digest
         FROM upload_session WHERE id = ?",
    )
    .bind(status_id.to_string())
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(pending, (0, 3, Sha256::digest(b"abc").to_vec()));

    let app = harness.app.clone();
    let status_request = request(
        "GET",
        &format!("/api/v1/uploads/{status_id}"),
        Body::empty(),
    );
    let status_task = tokio::spawn(async move { app.oneshot(status_request).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        !status_task.is_finished(),
        "status bypassed the live writer lease"
    );
    harness.staging.block_write.store(false, Ordering::SeqCst);
    harness.staging.release_write.notify_one();
    assert_eq!(writer.await.unwrap().status(), StatusCode::NO_CONTENT);
    let status_response = status_task.await.unwrap();
    assert_eq!(status_response.status(), StatusCode::OK);
    assert_eq!(response_json(status_response).await["committedOffset"], "3");

    let duplicate_id = created_id(&harness, &token, "blocked-duplicate.bin", "3").await;
    harness.staging.block_write.store(true, Ordering::SeqCst);
    let app = harness.app.clone();
    let first_request = chunk_request(&token, duplicate_id, "0", b"xyz");
    let first = tokio::spawn(async move { app.oneshot(first_request).await.unwrap() });
    harness.staging.write_started.notified().await;
    let app = harness.app.clone();
    let duplicate_request = chunk_request(&token, duplicate_id, "0", b"xyz");
    let duplicate = tokio::spawn(async move { app.oneshot(duplicate_request).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        !duplicate.is_finished(),
        "duplicate PUT bypassed the live writer lease"
    );
    harness.staging.block_write.store(false, Ordering::SeqCst);
    harness.staging.release_write.notify_one();
    assert_eq!(first.await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(duplicate.await.unwrap().status(), StatusCode::NO_CONTENT);
    let committed: (i64, Option<i64>, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT committed_offset, pending_offset, pending_digest
         FROM upload_session WHERE id = ?",
    )
    .bind(duplicate_id.to_string())
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(committed, (3, None, None));
    assert_eq!(harness.staging.bytes(duplicate_id), b"xyz");

    let cancel_id = created_id(&harness, &token, "blocked-cancel.bin", "3").await;
    harness.staging.block_write.store(true, Ordering::SeqCst);
    let app = harness.app.clone();
    let put_request = chunk_request(&token, cancel_id, "0", b"end");
    let writer = tokio::spawn(async move { app.oneshot(put_request).await.unwrap() });
    harness.staging.write_started.notified().await;
    let mut cancel_request = request(
        "DELETE",
        &format!("/api/v1/uploads/{cancel_id}"),
        Body::empty(),
    );
    authorize_mutation(&mut cancel_request, &token);
    let app = harness.app.clone();
    let cancel = tokio::spawn(async move { app.oneshot(cancel_request).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        !cancel.is_finished(),
        "cancel bypassed the live writer lease"
    );
    harness.staging.block_write.store(false, Ordering::SeqCst);
    harness.staging.release_write.notify_one();
    assert_eq!(writer.await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(cancel.await.unwrap().status(), StatusCode::NO_CONTENT);
    let cancelled: (String, i64, Option<i64>) = sqlx::query_as(
        "SELECT state, committed_offset, pending_offset FROM upload_session WHERE id = ?",
    )
    .bind(cancel_id.to_string())
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(cancelled, ("cancelled".to_owned(), 3, None));
    assert!(
        !harness
            .staging
            .files
            .lock()
            .unwrap()
            .contains_key(&cancel_id)
    );
    harness.pool.close().await;
}

#[tokio::test]
async fn startup_maintenance_releases_db_committed_creation_with_missing_staging() {
    let mut limits = default_limits();
    limits.max_active_sessions = 1;
    let harness = make_harness(limits, 1_000).await;
    let token = csrf_token(&harness.app).await;
    sqlx::query(
        "CREATE TRIGGER injected_fail_creation_cleanup
         BEFORE UPDATE OF state ON upload_session
         WHEN OLD.state = 'created' AND NEW.state = 'failed'
         BEGIN SELECT RAISE(ABORT, 'injected cleanup failure'); END",
    )
    .execute(&harness.pool)
    .await
    .unwrap();
    harness.staging.fail_create.store(true, Ordering::SeqCst);
    assert_error(
        create_upload(&harness, &token, "crash-window.bin", "1").await,
        StatusCode::SERVICE_UNAVAILABLE,
        "upload_unavailable",
    )
    .await;
    let stranded: (String, String) = sqlx::query_as(
        "SELECT id, state FROM upload_session WHERE destination_name = 'crash-window.bin'",
    )
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(stranded.1, "created");
    let stranded_id: UploadId = stranded.0.parse().unwrap();
    assert!(
        !harness
            .staging
            .files
            .lock()
            .unwrap()
            .contains_key(&stranded_id)
    );

    sqlx::query("DROP TRIGGER injected_fail_creation_cleanup")
        .execute(&harness.pool)
        .await
        .unwrap();
    let restarted = UploadService::new(
        Arc::new(SqliteUploadRepository::new(harness.pool.clone())),
        harness.staging.clone(),
        limits,
    );
    restarted
        .maintain(OffsetDateTime::from_unix_timestamp(NOW).unwrap())
        .await
        .unwrap();
    let old_state: String = sqlx::query_scalar("SELECT state FROM upload_session WHERE id = ?")
        .bind(stranded_id.to_string())
        .fetch_one(&harness.pool)
        .await
        .unwrap();
    assert_eq!(old_state, "failed");

    let replacement = create_upload(&harness, &token, "crash-window.bin", "1").await;
    assert_eq!(replacement.status(), StatusCode::CREATED);
    let replacement_id: UploadId = response_json(replacement).await["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_ne!(replacement_id, stranded_id);
    assert_eq!(harness.staging.files.lock().unwrap().len(), 1);
    assert!(
        harness
            .staging
            .files
            .lock()
            .unwrap()
            .contains_key(&replacement_id)
    );
    harness.pool.close().await;
}

#[tokio::test]
async fn partial_chunk_body_idle_timeout_is_bounded_and_does_not_record_pending_work() {
    use futures_util::StreamExt as _;

    let harness = make_harness_with_idle_timeout(
        default_limits(),
        1_000,
        Some(std::time::Duration::from_millis(20)),
    )
    .await;
    let token = csrf_token(&harness.app).await;
    let id = created_id(&harness, &token, "stalled.bin", "3").await;
    let stream = futures_util::stream::once(async {
        Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"a"))
    })
    .chain(futures_util::stream::pending());
    let mut request = request(
        "PUT",
        &format!("/api/v1/uploads/{id}/chunk"),
        Body::from_stream(stream),
    );
    request.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    request
        .headers_mut()
        .insert(header::CONTENT_LENGTH, "3".parse().unwrap());
    request.headers_mut().insert(
        HeaderName::from_static("upload-offset"),
        "0".parse().unwrap(),
    );
    let digest = STANDARD.encode(Sha256::digest(b"abc"));
    request.headers_mut().insert(
        HeaderName::from_static("digest"),
        format!("sha-256={digest}").parse().unwrap(),
    );
    authorize_mutation(&mut request, &token);
    let response = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        harness.app.clone().oneshot(request),
    )
    .await
    .expect("idle timeout must bound a stalled body")
    .unwrap();
    assert_error(response, StatusCode::REQUEST_TIMEOUT, "upload_body_timeout").await;
    let row: (i64, Option<i64>, String) = sqlx::query_as(
        "SELECT committed_offset, pending_offset, state FROM upload_session WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(row, (0, None, "created".to_owned()));
    assert!(harness.staging.bytes(id).is_empty());
    harness.pool.close().await;
}

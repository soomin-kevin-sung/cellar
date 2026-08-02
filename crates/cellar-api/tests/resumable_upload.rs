use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderName, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use cellar_api::routes::session::session_router_with_routes;
use cellar_api::routes::uploads::{MAX_UPLOAD_CHUNK_BYTES, uploads_router_with_clock};
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
}

impl MemoryStaging {
    fn with_space(bytes: i64) -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            available: AtomicI64::new(bytes),
            short_write: AtomicBool::new(false),
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
    let protected = uploads_router_with_clock::<EnrolledStore, _>(service, || {
        OffsetDateTime::from_unix_timestamp(NOW).unwrap()
    });
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
    harness.app.clone().oneshot(request).await.unwrap()
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
    assert_error(
        chunk(&harness, &token, second, "0", b"12").await,
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

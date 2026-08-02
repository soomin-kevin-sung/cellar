use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderName, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use cellar_api::routes::session::session_router_with_routes;
use cellar_api::routes::uploads::uploads_router_with_clock;
use cellar_auth::{
    AccessClaims, CompareAndSet, CsrfManager, EnrollmentService, EnrollmentSnapshot,
    EnrollmentStore, EnrollmentStoreError,
};
use cellar_core::{
    PublicationPresence, PublishedUpload, StagingIdentity, UploadCommitIntent, UploadId,
    UploadLimits, UploadPublicationError, UploadPublicationObservation, UploadPublisher,
    UploadService, UploadStagingError, UploadStagingStore, VerifiedUpload,
};
use cellar_db::{
    FilenameCollation, SqliteOperationRepository, SqliteUploadRepository, migrate, open_pool,
};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
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

#[derive(Clone)]
struct Destination {
    identity: StagingIdentity,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct MemoryPublication {
    staging: Mutex<HashMap<UploadId, (StagingIdentity, Vec<u8>)>>,
    destinations: Mutex<HashMap<String, Destination>>,
    available: AtomicI64,
    fail_after_publish: AtomicBool,
    race_before_publish: AtomicBool,
}

impl MemoryPublication {
    fn with_space(space: i64) -> Self {
        Self {
            available: AtomicI64::new(space),
            ..Self::default()
        }
    }

    fn key(intent: &UploadCommitIntent) -> String {
        format!(
            "{}/{}/{}",
            intent.project_id,
            intent.destination_components.join("/"),
            intent.destination_name
        )
    }

    fn identity(id: UploadId) -> StagingIdentity {
        let digest = Sha256::digest(id.to_string().as_bytes());
        let mut bytes = [0_u8; 24];
        bytes.copy_from_slice(&digest[..24]);
        StagingIdentity::new(bytes)
    }
}

#[async_trait]
impl UploadStagingStore for MemoryPublication {
    async fn create(&self, id: UploadId) -> Result<(), UploadStagingError> {
        let mut staging = self.staging.lock().unwrap();
        if staging
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
        let (_, bytes) = staging.get_mut(&id).ok_or(UploadStagingError::NotFound)?;
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
        let mut staging = self.staging.lock().unwrap();
        let (_, bytes) = staging.get_mut(&id).ok_or(UploadStagingError::NotFound)?;
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
        Ok(self.available.load(Ordering::SeqCst))
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
impl UploadPublisher for MemoryPublication {
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
        let staging = self.staging.lock().unwrap();
        let source =
            staging
                .get(&intent.upload_id)
                .map_or(PublicationPresence::Absent, |(identity, _)| {
                    if *identity == intent.staging_identity {
                        PublicationPresence::Expected
                    } else {
                        PublicationPresence::Unexpected
                    }
                });
        drop(staging);
        let destination = self.destinations.lock().unwrap();
        let destination = destination.get(&Self::key(intent)).map_or(
            PublicationPresence::Absent,
            |destination| {
                if destination.identity == intent.staging_identity {
                    PublicationPresence::Expected
                } else {
                    PublicationPresence::Unexpected
                }
            },
        );
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
        if self.race_before_publish.swap(false, Ordering::SeqCst) {
            destinations.insert(
                key.clone(),
                Destination {
                    identity: StagingIdentity::new([8; 24]),
                    bytes: b"racer".to_vec(),
                },
            );
        }
        if destinations.contains_key(&key) {
            return Err(UploadPublicationError::Conflict);
        }
        let (identity, bytes) = self
            .staging
            .lock()
            .unwrap()
            .remove(&intent.upload_id)
            .ok_or(UploadPublicationError::NotFound)?;
        if identity != intent.staging_identity {
            return Err(UploadPublicationError::Conflict);
        }
        let size = i64::try_from(bytes.len()).map_err(|_| UploadPublicationError::Unavailable)?;
        destinations.insert(key, Destination { identity, bytes });
        if self.fail_after_publish.swap(false, Ordering::SeqCst) {
            return Err(UploadPublicationError::Unavailable);
        }
        Ok(PublishedUpload {
            identity,
            size,
            mtime_filetime_100ns: 123,
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
        if destination.identity != intent.staging_identity {
            return Err(UploadPublicationError::Conflict);
        }
        Ok(PublishedUpload {
            identity: destination.identity,
            size: i64::try_from(destination.bytes.len())
                .map_err(|_| UploadPublicationError::Unavailable)?,
            mtime_filetime_100ns: 123,
        })
    }
}

struct Harness {
    app: axum::Router,
    pool: sqlx::SqlitePool,
    publication: Arc<MemoryPublication>,
    project_id: cellar_core::ProjectId,
    _directory: TempDir,
}

fn limits() -> UploadLimits {
    UploadLimits {
        max_chunk_size: 1024,
        max_active_sessions: 8,
        max_concurrent_uploads: 3,
        free_space_reserve: 10,
        session_ttl: Duration::days(7),
    }
}

async fn harness() -> Harness {
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
         VALUES (?, 'finalize', '', 'active', 1,
                 '1970-01-01T00:00:00.000000000Z',
                 '1970-01-01T00:00:00.000000000Z')",
    )
    .bind(project_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    let publication = Arc::new(MemoryPublication::with_space(1_000));
    let app = app(&pool, publication.clone());
    Harness {
        app,
        pool,
        publication,
        project_id,
        _directory: directory,
    }
}

fn app(pool: &sqlx::SqlitePool, publication: Arc<MemoryPublication>) -> axum::Router {
    let service = UploadService::with_finalization(
        Arc::new(SqliteUploadRepository::new(pool.clone())),
        publication.clone(),
        Arc::new(SqliteOperationRepository::new(pool.clone())),
        publication,
        limits(),
    );
    let protected = uploads_router_with_clock::<EnrolledStore, _>(service, || {
        OffsetDateTime::from_unix_timestamp(NOW).unwrap()
    });
    session_router_with_routes(
        EnrollmentService::new(Arc::new(EnrolledStore)),
        Arc::new(CsrfManager::new()),
        || NOW,
        protected,
    )
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

async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

async fn csrf(app: &axum::Router) -> String {
    let response = app
        .clone()
        .oneshot(request("GET", "/api/v1/session", Body::empty()))
        .await
        .unwrap();
    json_body(response).await["csrf_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn authorize(request: &mut Request<Body>, csrf: &str) {
    request
        .headers_mut()
        .insert(header::ORIGIN, ORIGIN.parse().unwrap());
    request.headers_mut().insert(
        HeaderName::from_static("x-cellar-csrf"),
        csrf.parse().unwrap(),
    );
    request
        .headers_mut()
        .insert("x-request-id", "finalize-request".parse().unwrap());
}

async fn create_upload(
    harness: &Harness,
    csrf: &str,
    name: &str,
    expected_hash: Option<[u8; 32]>,
) -> UploadId {
    let mut body = json!({
        "projectId": harness.project_id.to_string(),
        "destinationName": name,
        "expectedSize": "3"
    });
    if let Some(hash) = expected_hash {
        body["expectedHash"] = Value::String(STANDARD.encode(hash));
    }
    let mut request = request("POST", "/api/v1/uploads", Body::from(body.to_string()));
    request
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    authorize(&mut request, csrf);
    let response = harness.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    json_body(response).await["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

async fn upload_bytes(harness: &Harness, csrf: &str, id: UploadId, bytes: &[u8]) {
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
    request
        .headers_mut()
        .insert("upload-offset", "0".parse().unwrap());
    request.headers_mut().insert(
        "digest",
        format!("sha-256={}", STANDARD.encode(Sha256::digest(bytes)))
            .parse()
            .unwrap(),
    );
    authorize(&mut request, csrf);
    assert_eq!(
        harness.app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
}

async fn finalize(app: &axum::Router, csrf: &str, id: UploadId) -> axum::response::Response {
    let mut request = request(
        "POST",
        &format!("/api/v1/uploads/{id}/finalize"),
        Body::empty(),
    );
    authorize(&mut request, csrf);
    app.clone().oneshot(request).await.unwrap()
}

async fn assert_error(response: axum::response::Response, status: StatusCode, code: &str) {
    assert_eq!(response.status(), status);
    assert_eq!(response.headers()["x-request-id"], "finalize-request");
    let body = json_body(response).await;
    assert_eq!(body["code"], code);
    assert_eq!(body["requestId"], "finalize-request");
    assert_eq!(body["details"], json!({}));
}

#[tokio::test]
async fn finalizes_once_and_replays_the_same_catalog_entry() {
    let harness = harness().await;
    let token = csrf(&harness.app).await;
    let id = create_upload(&harness, &token, "complete.bin", None).await;
    upload_bytes(&harness, &token, id, b"abc").await;

    let first = finalize(&harness.app, &token, id).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(first.headers()["x-request-id"], "finalize-request");
    let first = json_body(first).await;
    assert_eq!(first["name"], "complete.bin");
    assert_eq!(first["size"], "3");
    assert_eq!(first["sha256"], STANDARD.encode(Sha256::digest(b"abc")));

    let replay = finalize(&harness.app, &token, id).await;
    assert_eq!(replay.status(), StatusCode::OK);
    let replay = json_body(replay).await;
    assert_eq!(replay["fileId"], first["fileId"]);
    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM file_entry),
                (SELECT count(*) FROM operation WHERE state = 'complete'),
                (SELECT count(*) FROM upload_session WHERE state = 'complete')",
    )
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(counts, (1, 1, 1));
    let payload: String =
        sqlx::query_scalar("SELECT payload FROM operation WHERE kind = 'upload_finalize'")
            .fetch_one(&harness.pool)
            .await
            .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&payload).unwrap()["resultIdentity"]
            .as_str()
            .map(str::len),
        Some(48)
    );
    harness.pool.close().await;
}

#[tokio::test]
async fn hash_mismatch_and_incomplete_upload_never_create_an_intent() {
    let harness = harness().await;
    let token = csrf(&harness.app).await;
    let mismatched = create_upload(&harness, &token, "hash.bin", Some([7; 32])).await;
    upload_bytes(&harness, &token, mismatched, b"abc").await;
    assert_error(
        finalize(&harness.app, &token, mismatched).await,
        StatusCode::CONFLICT,
        "upload_conflict",
    )
    .await;
    let incomplete = create_upload(&harness, &token, "short.bin", None).await;
    assert_error(
        finalize(&harness.app, &token, incomplete).await,
        StatusCode::CONFLICT,
        "upload_conflict",
    )
    .await;
    let operations: i64 = sqlx::query_scalar("SELECT count(*) FROM operation")
        .fetch_one(&harness.pool)
        .await
        .unwrap();
    assert_eq!(operations, 0);
    harness.pool.close().await;
}

#[tokio::test]
async fn restart_after_rename_completes_exactly_once() {
    let harness = harness().await;
    let token = csrf(&harness.app).await;
    let id = create_upload(&harness, &token, "restart.bin", None).await;
    upload_bytes(&harness, &token, id, b"abc").await;
    harness
        .publication
        .fail_after_publish
        .store(true, Ordering::SeqCst);
    assert_error(
        finalize(&harness.app, &token, id).await,
        StatusCode::SERVICE_UNAVAILABLE,
        "upload_unavailable",
    )
    .await;
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM operation WHERE kind = 'upload_finalize' AND state = 'pending'",
    )
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(pending, 1);
    let status = harness
        .app
        .clone()
        .oneshot(request(
            "GET",
            &format!("/api/v1/uploads/{id}"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let token_for_chunk = &token;
    let mut late_chunk = request(
        "PUT",
        &format!("/api/v1/uploads/{id}/chunk"),
        Body::from("abc"),
    );
    late_chunk.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    late_chunk
        .headers_mut()
        .insert(header::CONTENT_LENGTH, "3".parse().unwrap());
    late_chunk
        .headers_mut()
        .insert("upload-offset", "0".parse().unwrap());
    late_chunk.headers_mut().insert(
        "digest",
        format!("sha-256={}", STANDARD.encode(Sha256::digest(b"abc")))
            .parse()
            .unwrap(),
    );
    authorize(&mut late_chunk, token_for_chunk);
    assert_error(
        harness.app.clone().oneshot(late_chunk).await.unwrap(),
        StatusCode::CONFLICT,
        "upload_conflict",
    )
    .await;
    let durable_states: (String, String) = sqlx::query_as(
        "SELECT u.state, o.state FROM upload_session AS u
         JOIN upload_finalization AS f ON f.upload_id = u.id
         JOIN operation AS o ON o.id = f.operation_id WHERE u.id = ?",
    )
    .bind(id.to_string())
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(durable_states, ("committing".into(), "pending".into()));

    let restarted = app(&harness.pool, harness.publication.clone());
    let service = UploadService::with_finalization(
        Arc::new(SqliteUploadRepository::new(harness.pool.clone())),
        harness.publication.clone(),
        Arc::new(SqliteOperationRepository::new(harness.pool.clone())),
        harness.publication.clone(),
        limits(),
    );
    service
        .initialize(OffsetDateTime::from_unix_timestamp(NOW).unwrap())
        .await
        .unwrap();
    let restarted_token = csrf(&restarted).await;
    let response = finalize(&restarted, &restarted_token, id).await;
    assert_eq!(response.status(), StatusCode::OK);
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM file_entry),
                (SELECT count(*) FROM operation WHERE state = 'complete')",
    )
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(counts, (1, 1));
    harness.pool.close().await;
}

#[tokio::test]
async fn archived_deleted_and_low_space_finalization_fail_with_stable_envelopes() {
    let harness = harness().await;
    let token = csrf(&harness.app).await;
    let archived = create_upload(&harness, &token, "archived.bin", None).await;
    upload_bytes(&harness, &token, archived, b"abc").await;
    sqlx::query("UPDATE project SET status = 'archived' WHERE id = ?")
        .bind(harness.project_id.to_string())
        .execute(&harness.pool)
        .await
        .unwrap();
    assert_error(
        finalize(&harness.app, &token, archived).await,
        StatusCode::CONFLICT,
        "upload_conflict",
    )
    .await;
    sqlx::query("UPDATE project SET status = 'active', deleted_at = updated_at WHERE id = ?")
        .bind(harness.project_id.to_string())
        .execute(&harness.pool)
        .await
        .unwrap();
    assert_error(
        finalize(&harness.app, &token, archived).await,
        StatusCode::NOT_FOUND,
        "upload_not_found",
    )
    .await;
    sqlx::query("UPDATE project SET deleted_at = NULL WHERE id = ?")
        .bind(harness.project_id.to_string())
        .execute(&harness.pool)
        .await
        .unwrap();
    harness.publication.available.store(9, Ordering::SeqCst);
    assert_error(
        finalize(&harness.app, &token, archived).await,
        StatusCode::INSUFFICIENT_STORAGE,
        "insufficient_storage",
    )
    .await;
    harness.pool.close().await;
}

#[tokio::test]
async fn finalize_is_csrf_protected_and_method_bounded() {
    let harness = harness().await;
    let token = csrf(&harness.app).await;
    let id = create_upload(&harness, &token, "boundary.bin", None).await;
    let response = harness
        .app
        .clone()
        .oneshot(request(
            "POST",
            &format!("/api/v1/uploads/{id}/finalize"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let mut get = request(
        "GET",
        &format!("/api/v1/uploads/{id}/finalize"),
        Body::empty(),
    );
    get.headers_mut()
        .insert("x-request-id", "finalize-request".parse().unwrap());
    assert_error(
        harness.app.clone().oneshot(get).await.unwrap(),
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
    )
    .await;
    harness.pool.close().await;
}

#[tokio::test]
async fn an_unexpected_destination_is_preserved_and_fails_the_journal_visibly() {
    let harness = harness().await;
    let token = csrf(&harness.app).await;
    let id = create_upload(&harness, &token, "conflict.bin", None).await;
    upload_bytes(&harness, &token, id, b"abc").await;
    let key = format!("{}//conflict.bin", harness.project_id);
    harness.publication.destinations.lock().unwrap().insert(
        key,
        Destination {
            identity: StagingIdentity::new([9; 24]),
            bytes: b"other".to_vec(),
        },
    );

    assert_error(
        finalize(&harness.app, &token, id).await,
        StatusCode::CONFLICT,
        "upload_conflict",
    )
    .await;
    assert!(
        harness
            .publication
            .staging
            .lock()
            .unwrap()
            .contains_key(&id)
    );
    assert_eq!(
        harness
            .publication
            .destinations
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .bytes,
        b"other"
    );
    let states: (String, String, i64) = sqlx::query_as(
        "SELECT u.state, o.state, (SELECT count(*) FROM file_entry)
         FROM upload_session AS u JOIN upload_finalization AS f ON f.upload_id = u.id
         JOIN operation AS o ON o.id = f.operation_id WHERE u.id = ?",
    )
    .bind(id.to_string())
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(states, ("failed".into(), "failed".into(), 0));
    harness.pool.close().await;
}

#[tokio::test]
async fn upload_intents_reject_windows_unsafe_destination_names_before_staging() {
    let harness = harness().await;
    let token = csrf(&harness.app).await;
    for name in ["bad*.bin", "CON.txt", "trailing. "] {
        let mut request = request(
            "POST",
            "/api/v1/uploads",
            Body::from(
                json!({
                    "projectId": harness.project_id.to_string(),
                    "destinationName": name,
                    "expectedSize": "3"
                })
                .to_string(),
            ),
        );
        request
            .headers_mut()
            .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        authorize(&mut request, &token);
        assert_error(
            harness.app.clone().oneshot(request).await.unwrap(),
            StatusCode::BAD_REQUEST,
            "invalid_upload_request",
        )
        .await;
    }
    assert!(harness.publication.staging.lock().unwrap().is_empty());
    harness.pool.close().await;
}

#[tokio::test]
async fn a_destination_race_after_intent_is_preserved_and_terminally_reconciled() {
    let harness = harness().await;
    let token = csrf(&harness.app).await;
    let id = create_upload(&harness, &token, "race.bin", None).await;
    upload_bytes(&harness, &token, id, b"abc").await;
    harness
        .publication
        .race_before_publish
        .store(true, Ordering::SeqCst);
    assert_error(
        finalize(&harness.app, &token, id).await,
        StatusCode::CONFLICT,
        "upload_conflict",
    )
    .await;
    let states: (String, String) = sqlx::query_as(
        "SELECT u.state, o.state FROM upload_session AS u
         JOIN upload_finalization AS f ON f.upload_id = u.id
         JOIN operation AS o ON o.id = f.operation_id WHERE u.id = ?",
    )
    .bind(id.to_string())
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(states, ("failed".into(), "failed".into()));
    assert!(
        harness
            .publication
            .staging
            .lock()
            .unwrap()
            .contains_key(&id)
    );
    harness.pool.close().await;
}

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::{Body, Bytes};
use axum::http::{HeaderName, Request, StatusCode, header};
use cellar_api::routes::projects::{
    MAX_PROJECT_BODY_BYTES, parse_decimal_version, projects_router_with_clock,
};
use cellar_api::routes::session::session_router_with_routes;
use cellar_auth::{
    AccessClaims, CompareAndSet, CsrfManager, EnrollmentService, EnrollmentSnapshot,
    EnrollmentStore, EnrollmentStoreError,
};
use cellar_core::{
    DirectoryStoreError, MAX_PROJECT_DESCRIPTION_BYTES, MAX_PROJECT_NAME_BYTES, NewProject,
    OperationId, OperationStart, Project, ProjectDirectoryStore, ProjectId, ProjectListFilter,
    ProjectPatch, ProjectRepository, ProjectRepositoryError, ProjectService, ProjectServiceError,
    ProjectStatus,
};
use cellar_db::{FilenameCollation, SqliteProjectRepository, migrate, open_pool};
use futures_util::stream;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tempfile::TempDir;
use time::OffsetDateTime;
use tower::ServiceExt;

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

struct RecordingDirectories {
    pool: SqlitePool,
    calls: AtomicUsize,
    outcome: Option<DirectoryStoreError>,
}

struct FailingMarkRepository {
    inner: Arc<SqliteProjectRepository>,
}

#[async_trait]
impl ProjectRepository for FailingMarkRepository {
    async fn begin_create(
        &self,
        operation_id: OperationId,
        project: &Project,
        request_digest: &str,
    ) -> Result<OperationStart, ProjectRepositoryError> {
        self.inner
            .begin_create(operation_id, project, request_digest)
            .await
    }

    async fn mark_create_fs_applied(
        &self,
        operation_id: OperationId,
        now: OffsetDateTime,
    ) -> Result<(), ProjectRepositoryError> {
        self.inner.mark_create_fs_applied(operation_id, now).await
    }

    async fn mark_create_failed(
        &self,
        _: OperationId,
        _: &'static str,
        _: OffsetDateTime,
    ) -> Result<(), ProjectRepositoryError> {
        Err(ProjectRepositoryError::Unavailable)
    }

    async fn create(
        &self,
        operation_id: OperationId,
        project: &Project,
    ) -> Result<Project, ProjectRepositoryError> {
        self.inner.create(operation_id, project).await
    }

    async fn recover_create(
        &self,
        operation_id: OperationId,
    ) -> Result<Project, ProjectRepositoryError> {
        self.inner.recover_create(operation_id).await
    }

    async fn read(&self, id: ProjectId) -> Result<Project, ProjectRepositoryError> {
        self.inner.read(id).await
    }

    async fn list(
        &self,
        filter: ProjectListFilter,
        limit: u32,
    ) -> Result<Vec<Project>, ProjectRepositoryError> {
        self.inner.list(filter, limit).await
    }

    async fn update(
        &self,
        id: ProjectId,
        expected_version: i64,
        patch: &ProjectPatch,
        now: OffsetDateTime,
    ) -> Result<Project, ProjectRepositoryError> {
        self.inner.update(id, expected_version, patch, now).await
    }

    async fn archive(
        &self,
        id: ProjectId,
        expected_version: i64,
        now: OffsetDateTime,
    ) -> Result<Project, ProjectRepositoryError> {
        self.inner.archive(id, expected_version, now).await
    }
}

#[async_trait]
impl ProjectDirectoryStore for RecordingDirectories {
    async fn create_project_directory(
        &self,
        _: cellar_core::ProjectId,
    ) -> Result<(), DirectoryStoreError> {
        let state: String = sqlx::query_scalar(
            "SELECT state FROM operation WHERE kind = 'project_create' ORDER BY created_at DESC",
        )
        .fetch_one(&self.pool)
        .await
        .expect("pending journal is durable before directory mutation");
        assert_eq!(state, "pending");
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.outcome.map_or(Ok(()), Err)
    }
}

async fn test_service(
    outcome: Option<DirectoryStoreError>,
) -> (
    TempDir,
    SqlitePool,
    ProjectService,
    Arc<RecordingDirectories>,
) {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("cellar.db");
    let collation = FilenameCollation::windows_ordinal_ci_v1(|left, right| left.cmp(right));
    let pool = open_pool(path, collation).await.unwrap();
    migrate(&pool).await.unwrap();
    let repository = Arc::new(SqliteProjectRepository::new(pool.clone()));
    let directories = Arc::new(RecordingDirectories {
        pool: pool.clone(),
        calls: AtomicUsize::new(0),
        outcome,
    });
    let service = ProjectService::new(repository, directories.clone());
    (directory, pool, service, directories)
}

fn request(method: &str, uri: &str, body: Body, authenticated: bool) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .body(body)
        .unwrap();
    if authenticated {
        request.extensions_mut().insert(claims());
    }
    request
}

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    let mut request = request(method, uri, Body::from(body.to_string()), true);
    request
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    request
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

async fn assert_project_error(response: axum::response::Response, status: StatusCode, code: &str) {
    assert_eq!(response.status(), status);
    assert_eq!(
        response.headers()[HeaderName::from_static("content-type")],
        "application/json"
    );
    let body = response_json(response).await;
    assert_eq!(body["code"], code);
    assert!(body["requestId"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(body["details"], json!({}));
}

async fn app() -> (axum::Router, TempDir, SqlitePool, Arc<RecordingDirectories>) {
    app_with_directory_outcome(None).await
}

async fn app_with_directory_outcome(
    outcome: Option<DirectoryStoreError>,
) -> (axum::Router, TempDir, SqlitePool, Arc<RecordingDirectories>) {
    let (directory, pool, service, directories) = test_service(outcome).await;
    let protected = projects_router_with_clock::<EnrolledStore, _>(service, || {
        OffsetDateTime::from_unix_timestamp(NOW).unwrap()
    });
    let app = session_router_with_routes(
        EnrollmentService::new(Arc::new(EnrolledStore)),
        Arc::new(CsrfManager::new()),
        || NOW,
        protected,
    );
    (app, directory, pool, directories)
}

async fn csrf_token(app: &axum::Router) -> String {
    let response = app
        .clone()
        .oneshot(request("GET", "/api/v1/session", Body::empty(), true))
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

#[test]
fn project_input_contract_rejects_invalid_values() {
    assert!(NewProject::try_new("", "").is_err());
    assert!(NewProject::try_new(" leading", "").is_err());
    assert!(NewProject::try_new("trailing ", "").is_err());
    assert!(NewProject::try_new("bad\0name", "").is_err());
    assert!(NewProject::try_new("bad\u{7f}name", "").is_err());
    assert!(NewProject::try_new("x".repeat(MAX_PROJECT_NAME_BYTES + 1), "").is_err());
    assert!(NewProject::try_new("valid", "x".repeat(MAX_PROJECT_DESCRIPTION_BYTES + 1)).is_err());
    assert!(NewProject::try_new("valid", "line one\nline two").is_ok());
    assert!(ProjectPatch::try_new(None, None).is_err());
}

#[test]
fn status_and_decimal_version_contract_is_canonical() {
    assert_eq!(ProjectStatus::Active.as_str(), "active");
    assert_eq!(ProjectStatus::Archived.as_str(), "archived");
    assert_eq!(parse_decimal_version("1"), Ok(1));
    assert_eq!(
        "deleted".parse::<ProjectStatus>().unwrap_err().code(),
        "invalid_project_status"
    );
    for invalid in ["", "0", "01", "+1", "-1", " 1", "1 ", "1.0"] {
        assert!(
            parse_decimal_version(invalid).is_err(),
            "accepted {invalid:?}"
        );
    }
}

#[tokio::test]
async fn concurrent_same_key_across_repository_instances_mutates_directory_once() {
    let (_directory, pool, first, directories) = test_service(None).await;
    let second = ProjectService::new(
        Arc::new(SqliteProjectRepository::new(pool.clone())),
        directories.clone(),
    );
    let operation = OperationId::new();
    let now = OffsetDateTime::from_unix_timestamp(NOW).unwrap();
    let first_request = first.create(
        NewProject::try_new("concurrent", "same").unwrap(),
        Some(operation),
        now,
    );
    let second_request = second.create(
        NewProject::try_new("concurrent", "same").unwrap(),
        Some(operation),
        now,
    );
    let (first_result, second_result) = tokio::join!(first_request, second_request);
    for result in [&first_result, &second_result] {
        assert!(
            result.is_ok() || result == &Err(ProjectServiceError::InProgress),
            "duplicate failed unsafely: {result:?}"
        );
    }
    assert!(first_result.is_ok() || second_result.is_ok());
    assert_eq!(directories.calls.load(Ordering::SeqCst), 1);
    let project_count: i64 = sqlx::query_scalar("SELECT count(*) FROM project")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(project_count, 1);
    pool.close().await;
}

#[tokio::test]
async fn real_http_lifecycle_replays_and_composes_with_auth_csrf_and_cors_boundary() {
    let (app, _directory, pool, directories) = app().await;
    let unauthenticated = app
        .clone()
        .oneshot(request("GET", "/api/v1/projects", Body::empty(), false))
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    assert!(
        unauthenticated
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );

    let token = csrf_token(&app).await;
    let operation = OperationId::new();
    let mut create = json_request(
        "POST",
        "/api/v1/projects",
        json!({"name":"Library","description":"First\nproject"}),
    );
    create.headers_mut().insert(
        HeaderName::from_static("idempotency-key"),
        operation.to_string().parse().unwrap(),
    );
    authorize_mutation(&mut create, &token);
    let response = app.clone().oneshot(create).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = response_json(response).await;
    assert_eq!(created["version"], "1");
    assert_eq!(created["status"], "active");
    assert!(created.get("deletedAt").is_none());
    assert!(created.get("path").is_none());
    let id = created["id"].as_str().unwrap();

    let mut replay = json_request(
        "POST",
        "/api/v1/projects",
        json!({"name":"Library","description":"First\nproject"}),
    );
    replay.headers_mut().insert(
        HeaderName::from_static("idempotency-key"),
        operation.to_string().parse().unwrap(),
    );
    authorize_mutation(&mut replay, &token);
    let replay = app.clone().oneshot(replay).await.unwrap();
    assert_eq!(replay.status(), StatusCode::CREATED);
    assert_eq!(response_json(replay).await["id"], id);
    assert_eq!(directories.calls.load(Ordering::SeqCst), 1);

    let mut mismatch = json_request(
        "POST",
        "/api/v1/projects",
        json!({"name":"Different","description":""}),
    );
    mismatch.headers_mut().insert(
        HeaderName::from_static("idempotency-key"),
        operation.to_string().parse().unwrap(),
    );
    authorize_mutation(&mut mismatch, &token);
    let mismatch = app.clone().oneshot(mismatch).await.unwrap();
    assert_eq!(mismatch.status(), StatusCode::CONFLICT);
    let mismatch = response_json(mismatch).await;
    assert_eq!(mismatch["code"], "idempotency_conflict");
    assert_eq!(mismatch["details"], json!({}));

    let get = app
        .clone()
        .oneshot(request(
            "GET",
            &format!("/api/v1/projects/{id}"),
            Body::empty(),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::OK);

    let mut update = json_request(
        "PATCH",
        &format!("/api/v1/projects/{id}"),
        json!({"expectedVersion":"1","name":"Renamed"}),
    );
    authorize_mutation(&mut update, &token);
    let updated = app.clone().oneshot(update).await.unwrap();
    assert_eq!(updated.status(), StatusCode::OK);
    assert_eq!(response_json(updated).await["version"], "2");

    let mut stale = json_request(
        "PATCH",
        &format!("/api/v1/projects/{id}"),
        json!({"expectedVersion":"1","description":"stale"}),
    );
    authorize_mutation(&mut stale, &token);
    assert_eq!(
        app.clone().oneshot(stale).await.unwrap().status(),
        StatusCode::CONFLICT
    );

    let mut archive = json_request(
        "POST",
        &format!("/api/v1/projects/{id}/archive"),
        json!({"expectedVersion":"2"}),
    );
    authorize_mutation(&mut archive, &token);
    let archived = app.clone().oneshot(archive).await.unwrap();
    assert_eq!(archived.status(), StatusCode::OK);
    let archived = response_json(archived).await;
    assert_eq!(archived["version"], "3");
    assert_eq!(archived["status"], "archived");

    let active = app
        .clone()
        .oneshot(request("GET", "/api/v1/projects", Body::empty(), true))
        .await
        .unwrap();
    assert_eq!(response_json(active).await, json!([]));
    let all = app
        .oneshot(request(
            "GET",
            "/api/v1/projects?status=all",
            Body::empty(),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(response_json(all).await.as_array().unwrap().len(), 1);
    pool.close().await;
}

#[tokio::test]
async fn malformed_json_headers_and_identifiers_are_stable_client_errors() {
    let (app, _directory, pool, _directories) = app().await;
    let token = csrf_token(&app).await;
    for body in [
        r#"{"name":"a","name":"b"}"#,
        r#"{"name":"a","unknown":true}"#,
    ] {
        let mut invalid = request("POST", "/api/v1/projects", Body::from(body), true);
        invalid
            .headers_mut()
            .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        authorize_mutation(&mut invalid, &token);
        assert_eq!(
            app.clone().oneshot(invalid).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
    }
    let missing = app
        .clone()
        .oneshot(request(
            "GET",
            "/api/v1/projects/not-a-uuid",
            Body::empty(),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

    let mut oversized = request(
        "POST",
        "/api/v1/projects",
        Body::from(vec![b'x'; MAX_PROJECT_BODY_BYTES + 1]),
        true,
    );
    oversized
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    authorize_mutation(&mut oversized, &token);
    assert_eq!(
        app.clone().oneshot(oversized).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );

    let mut duplicate_key = json_request(
        "POST",
        "/api/v1/projects",
        json!({"name":"duplicate header"}),
    );
    let idempotency = HeaderName::from_static("idempotency-key");
    duplicate_key.headers_mut().append(
        &idempotency,
        OperationId::new().to_string().parse().unwrap(),
    );
    duplicate_key.headers_mut().append(
        &idempotency,
        OperationId::new().to_string().parse().unwrap(),
    );
    authorize_mutation(&mut duplicate_key, &token);
    assert_eq!(
        app.clone().oneshot(duplicate_key).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );

    let mut no_csrf = json_request("POST", "/api/v1/projects", json!({"name":"blocked"}));
    no_csrf
        .headers_mut()
        .insert(header::ORIGIN, ORIGIN.parse().unwrap());
    assert_eq!(
        app.clone().oneshot(no_csrf).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    pool.close().await;
    let unavailable = app
        .oneshot(request("GET", "/api/v1/projects", Body::empty(), true))
        .await
        .unwrap();
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response_json(unavailable).await["code"],
        "project_service_unavailable"
    );
}

fn padded_json(value: &str, length: usize) -> Vec<u8> {
    assert!(value.len() <= length);
    let mut body = value.as_bytes().to_vec();
    body.resize(length, b' ');
    body
}

#[tokio::test]
async fn json_body_limits_are_stable_at_the_real_router_boundary() {
    let (app, _directory, pool, _directories) = app().await;
    let token = csrf_token(&app).await;
    let mut exact = request(
        "POST",
        "/api/v1/projects",
        Body::from(padded_json(r#"{"name":"exact"}"#, MAX_PROJECT_BODY_BYTES)),
        true,
    );
    exact
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    authorize_mutation(&mut exact, &token);
    assert_eq!(
        app.clone().oneshot(exact).await.unwrap().status(),
        StatusCode::CREATED
    );

    let id = ProjectId::new();
    for (method, uri, valid) in [
        (
            "PATCH",
            format!("/api/v1/projects/{id}"),
            r#"{"expectedVersion":"1","name":"x"}"#,
        ),
        (
            "POST",
            format!("/api/v1/projects/{id}/archive"),
            r#"{"expectedVersion":"1"}"#,
        ),
    ] {
        let mut exact = request(
            method,
            &uri,
            Body::from(padded_json(valid, MAX_PROJECT_BODY_BYTES)),
            true,
        );
        exact
            .headers_mut()
            .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        authorize_mutation(&mut exact, &token);
        assert_project_error(
            app.clone().oneshot(exact).await.unwrap(),
            StatusCode::NOT_FOUND,
            "project_not_found",
        )
        .await;
    }

    let chunked = stream::iter([
        Ok::<_, Infallible>(Bytes::from(padded_json(
            r#"{"name":"chunked"}"#,
            MAX_PROJECT_BODY_BYTES,
        ))),
        Ok(Bytes::from_static(b" ")),
    ]);
    let mut chunked = request("POST", "/api/v1/projects", Body::from_stream(chunked), true);
    assert!(chunked.headers().get(header::CONTENT_LENGTH).is_none());
    chunked
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    authorize_mutation(&mut chunked, &token);
    assert_project_error(
        app.clone().oneshot(chunked).await.unwrap(),
        StatusCode::BAD_REQUEST,
        "request_body_too_large",
    )
    .await;

    for (method, uri, valid) in [
        ("POST", "/api/v1/projects".to_owned(), r#"{"name":"x"}"#),
        (
            "PATCH",
            format!("/api/v1/projects/{id}"),
            r#"{"expectedVersion":"1","name":"x"}"#,
        ),
        (
            "POST",
            format!("/api/v1/projects/{id}/archive"),
            r#"{"expectedVersion":"1"}"#,
        ),
    ] {
        for length in [MAX_PROJECT_BODY_BYTES + 1, 2 * 1024 * 1024 + 1] {
            let mut oversized = request(method, &uri, Body::from(padded_json(valid, length)), true);
            assert!(oversized.headers().get(header::CONTENT_LENGTH).is_none());
            oversized
                .headers_mut()
                .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            authorize_mutation(&mut oversized, &token);
            assert_project_error(
                app.clone().oneshot(oversized).await.unwrap(),
                StatusCode::BAD_REQUEST,
                "request_body_too_large",
            )
            .await;
        }
    }
    pool.close().await;
}

#[tokio::test]
async fn malformed_raw_project_paths_and_nonexistent_ids_use_project_envelopes() {
    let (app, _directory, pool, _directories) = app().await;
    let token = csrf_token(&app).await;
    let id = ProjectId::new();
    for path in [
        "%FF".to_owned(),
        "%".to_owned(),
        "%2F".to_owned(),
        "%5C".to_owned(),
        String::new(),
        format!("{id}/"),
        format!("{id}/extra"),
        format!("{id}/extra/more"),
    ] {
        let uri = format!("/api/v1/projects/{path}");
        assert_project_error(
            app.clone()
                .oneshot(request("GET", &uri, Body::empty(), true))
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            "invalid_project_id",
        )
        .await;
    }

    assert_project_error(
        app.clone()
            .oneshot(request(
                "GET",
                &format!("/api/v1/projects/{id}"),
                Body::empty(),
                true,
            ))
            .await
            .unwrap(),
        StatusCode::NOT_FOUND,
        "project_not_found",
    )
    .await;
    for (method, uri, body) in [
        (
            "PATCH",
            format!("/api/v1/projects/{id}"),
            json!({"expectedVersion":"1","name":"missing"}),
        ),
        (
            "POST",
            format!("/api/v1/projects/{id}/archive"),
            json!({"expectedVersion":"1"}),
        ),
    ] {
        let mut missing = json_request(method, &uri, body);
        authorize_mutation(&mut missing, &token);
        assert_project_error(
            app.clone().oneshot(missing).await.unwrap(),
            StatusCode::NOT_FOUND,
            "project_not_found",
        )
        .await;
    }
    pool.close().await;
}

#[tokio::test]
async fn pending_and_fs_applied_replays_fail_closed_without_directory_mutation() {
    let (_directory, pool, service, directories) = test_service(None).await;
    let operation = OperationId::new();
    let now = OffsetDateTime::from_unix_timestamp(NOW).unwrap();
    let input = || NewProject::try_new("recovery", "same request").unwrap();
    let created = service
        .create(input(), Some(operation), now)
        .await
        .unwrap()
        .project;
    assert_eq!(directories.calls.load(Ordering::SeqCst), 1);

    for state in ["pending", "fs_applied"] {
        sqlx::query("UPDATE operation SET state = ? WHERE id = ?")
            .bind(state)
            .bind(operation.to_string())
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            service.create(input(), Some(operation), now).await,
            Err(ProjectServiceError::InProgress)
        );
        assert_eq!(directories.calls.load(Ordering::SeqCst), 1);
    }

    sqlx::query("UPDATE operation SET state = 'complete' WHERE id = ?")
        .bind(operation.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let replay = service.create(input(), Some(operation), now).await.unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.project, created);
    assert_eq!(directories.calls.load(Ordering::SeqCst), 1);
    pool.close().await;
}

#[tokio::test]
async fn terminal_storage_failures_replay_the_same_outward_error_without_retrying_directory() {
    for (outcome, status, code) in [
        (
            DirectoryStoreError::Conflict,
            StatusCode::CONFLICT,
            "project_destination_conflict",
        ),
        (
            DirectoryStoreError::Unavailable,
            StatusCode::SERVICE_UNAVAILABLE,
            "project_service_unavailable",
        ),
    ] {
        let (app, _directory, pool, directories) = app_with_directory_outcome(Some(outcome)).await;
        let token = csrf_token(&app).await;
        let operation = OperationId::new();
        for _ in 0..2 {
            let mut create = json_request(
                "POST",
                "/api/v1/projects",
                json!({"name":"terminal","description":"same"}),
            );
            create.headers_mut().insert(
                HeaderName::from_static("idempotency-key"),
                operation.to_string().parse().unwrap(),
            );
            authorize_mutation(&mut create, &token);
            assert_project_error(app.clone().oneshot(create).await.unwrap(), status, code).await;
        }
        assert_eq!(directories.calls.load(Ordering::SeqCst), 1);

        let mut mismatch = json_request(
            "POST",
            "/api/v1/projects",
            json!({"name":"terminal-different","description":"same"}),
        );
        mismatch.headers_mut().insert(
            HeaderName::from_static("idempotency-key"),
            operation.to_string().parse().unwrap(),
        );
        authorize_mutation(&mut mismatch, &token);
        assert_project_error(
            app.clone().oneshot(mismatch).await.unwrap(),
            StatusCode::CONFLICT,
            "idempotency_conflict",
        )
        .await;
        assert_eq!(directories.calls.load(Ordering::SeqCst), 1);

        sqlx::query("UPDATE operation SET error = ? WHERE id = ?")
            .bind("C:\\private\\must-not-leak")
            .bind(operation.to_string())
            .execute(&pool)
            .await
            .unwrap();
        let mut corrupt = json_request(
            "POST",
            "/api/v1/projects",
            json!({"name":"terminal","description":"same"}),
        );
        corrupt.headers_mut().insert(
            HeaderName::from_static("idempotency-key"),
            operation.to_string().parse().unwrap(),
        );
        authorize_mutation(&mut corrupt, &token);
        let corrupt = app.clone().oneshot(corrupt).await.unwrap();
        assert_project_error(
            corrupt,
            StatusCode::SERVICE_UNAVAILABLE,
            "project_service_unavailable",
        )
        .await;
        assert_eq!(directories.calls.load(Ordering::SeqCst), 1);
        pool.close().await;
    }
}

#[tokio::test]
async fn failed_terminal_journal_write_returns_503_and_leaves_pending_recovery_evidence() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("cellar.db");
    let collation = FilenameCollation::windows_ordinal_ci_v1(|left, right| left.cmp(right));
    let pool = open_pool(path, collation).await.unwrap();
    migrate(&pool).await.unwrap();
    let repository = Arc::new(SqliteProjectRepository::new(pool.clone()));
    let directories = Arc::new(RecordingDirectories {
        pool: pool.clone(),
        calls: AtomicUsize::new(0),
        outcome: Some(DirectoryStoreError::Conflict),
    });
    let service = ProjectService::new(
        Arc::new(FailingMarkRepository { inner: repository }),
        directories.clone(),
    );
    let protected = projects_router_with_clock::<EnrolledStore, _>(service, || {
        OffsetDateTime::from_unix_timestamp(NOW).unwrap()
    });
    let app = session_router_with_routes(
        EnrollmentService::new(Arc::new(EnrolledStore)),
        Arc::new(CsrfManager::new()),
        || NOW,
        protected,
    );
    let token = csrf_token(&app).await;
    let operation = OperationId::new();

    let send = || {
        let mut create = json_request(
            "POST",
            "/api/v1/projects",
            json!({"name":"journal failure","description":"same"}),
        );
        create.headers_mut().insert(
            HeaderName::from_static("idempotency-key"),
            operation.to_string().parse().unwrap(),
        );
        authorize_mutation(&mut create, &token);
        create
    };
    assert_project_error(
        app.clone().oneshot(send()).await.unwrap(),
        StatusCode::SERVICE_UNAVAILABLE,
        "project_service_unavailable",
    )
    .await;
    let state: String = sqlx::query_scalar("SELECT state FROM operation WHERE id = ?")
        .bind(operation.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "pending");
    assert_eq!(directories.calls.load(Ordering::SeqCst), 1);

    assert_project_error(
        app.clone().oneshot(send()).await.unwrap(),
        StatusCode::CONFLICT,
        "project_create_in_progress",
    )
    .await;
    assert_eq!(directories.calls.load(Ordering::SeqCst), 1);
    pool.close().await;
}

#[tokio::test]
async fn unsupported_project_methods_use_stable_json_after_auth_and_csrf() {
    let (app, _directory, pool, _directories) = app().await;
    let id = ProjectId::new();
    let anonymous = app
        .clone()
        .oneshot(request("PUT", "/api/v1/projects", Body::empty(), false))
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let no_csrf = app
        .clone()
        .oneshot(request("PUT", "/api/v1/projects", Body::empty(), true))
        .await
        .unwrap();
    assert_eq!(no_csrf.status(), StatusCode::FORBIDDEN);

    let token = csrf_token(&app).await;
    for (method, uri, csrf) in [
        ("PUT", "/api/v1/projects".to_owned(), true),
        ("DELETE", format!("/api/v1/projects/{id}"), true),
        ("DELETE", format!("/api/v1/projects/{id}/archive"), true),
        ("OPTIONS", "/api/v1/projects".to_owned(), false),
    ] {
        let mut unsupported = request(method, &uri, Body::empty(), true);
        if csrf {
            authorize_mutation(&mut unsupported, &token);
        }
        let response = app.clone().oneshot(unsupported).await.unwrap();
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
        assert_project_error(
            response,
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
        )
        .await;
    }
    pool.close().await;
}

#[tokio::test]
async fn failures_keep_sanitized_journal_recovery_evidence() {
    let (_directory, pool, service, directories) =
        test_service(Some(DirectoryStoreError::Conflict)).await;
    let error = service
        .create(
            NewProject::try_new("conflict", "").unwrap(),
            Some(OperationId::new()),
            OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(error, ProjectServiceError::Conflict);
    let failed: (String, Option<String>) = sqlx::query_as("SELECT state, error FROM operation")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        failed,
        ("failed".into(), Some("project_destination_conflict".into()))
    );
    assert_eq!(directories.calls.load(Ordering::SeqCst), 1);
    pool.close().await;

    let (_directory, pool, service, directories) = test_service(None).await;
    sqlx::query(
        "CREATE TRIGGER reject_project BEFORE INSERT ON project
         BEGIN SELECT RAISE(ABORT, 'injected'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        service
            .create(
                NewProject::try_new("fs-applied", "").unwrap(),
                Some(OperationId::new()),
                OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
            )
            .await
            .unwrap_err(),
        ProjectServiceError::Unavailable
    );
    let state: String = sqlx::query_scalar("SELECT state FROM operation")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "fs_applied");
    assert_eq!(directories.calls.load(Ordering::SeqCst), 1);
    pool.close().await;
}

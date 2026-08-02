use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderName, Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cellar_api::routes::files::files_router;
use cellar_api::routes::files::{MAX_FILE_CURSOR_BYTES, MAX_FILE_QUERY_BYTES};
use cellar_api::routes::session::session_router_with_routes;
use cellar_auth::{
    AccessClaims, CompareAndSet, CsrfManager, EnrollmentService, EnrollmentSnapshot,
    EnrollmentStore, EnrollmentStoreError,
};
use cellar_core::{FileEntryId, FileService, ProjectId};
use cellar_db::{FilenameCollation, SqliteFileRepository, migrate, open_pool};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tempfile::TempDir;
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

async fn fixture() -> (axum::Router, TempDir, SqlitePool, ProjectId) {
    let directory = TempDir::new().unwrap();
    let collation = FilenameCollation::windows_ordinal_ci_v1(|left, right| {
        left.to_lowercase().cmp(&right.to_lowercase())
    });
    let pool = open_pool(directory.path().join("cellar.db"), collation)
        .await
        .unwrap();
    migrate(&pool).await.unwrap();
    let project_id = ProjectId::new();
    insert_project(&pool, project_id, "active").await;
    let service = FileService::new(Arc::new(SqliteFileRepository::new(pool.clone())));
    let protected = files_router::<EnrolledStore>(service);
    let app = session_router_with_routes(
        EnrollmentService::new(Arc::new(EnrolledStore)),
        Arc::new(CsrfManager::new()),
        || NOW,
        protected,
    );
    (app, directory, pool, project_id)
}

async fn insert_project(pool: &SqlitePool, id: ProjectId, status: &str) {
    sqlx::query(
        "INSERT INTO project
         (id, name, description, status, version, created_at, updated_at)
         VALUES (?, 'Files', '', ?, 1, '2026-07-31T00:00:00Z', '2026-07-31T00:00:00Z')",
    )
    .bind(id.to_string())
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_entry(
    pool: &SqlitePool,
    id: FileEntryId,
    project_id: ProjectId,
    parent_id: Option<FileEntryId>,
    name: &str,
    kind: &str,
    state: &str,
) {
    sqlx::query(
        "INSERT INTO file_entry
         (id, project_id, parent_id, exact_name, kind, platform_kind, volume_serial,
          filesystem_file_id, size, mtime_filetime_100ns, hash, hash_state, state,
          revision, scan_generation, observed_at)
         VALUES (?, ?, ?, ?, ?, 'windows_file_id', ?, ?, 12, 1337, NULL, 'unknown',
                 ?, 1, ?, '2026-07-31T00:00:00Z')",
    )
    .bind(id.to_string())
    .bind(project_id.to_string())
    .bind(parent_id.map(|id| id.to_string()))
    .bind(name)
    .bind(kind)
    .bind([1_u8; 8].as_slice())
    .bind(id.to_string().as_bytes()[..16].to_vec())
    .bind(state)
    .bind(1_i64)
    .execute(pool)
    .await
    .unwrap();
}

fn request(uri: &str, authenticated: bool) -> Request<Body> {
    let mut request = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    if authenticated {
        request.extensions_mut().insert(claims());
    }
    request
}

async fn json(response: axum::response::Response) -> Value {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

async fn assert_file_error(response: axum::response::Response, status: StatusCode, code: &str) {
    assert_eq!(response.status(), status);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let body = json(response).await;
    assert_eq!(body["code"], code);
    assert!(body["requestId"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(body["details"], json!({}));
}

async fn first_cursor(app: &axum::Router, project_id: ProjectId) -> String {
    let response = app
        .clone()
        .oneshot(request(
            &format!("/api/v1/projects/{project_id}/files?limit=1"),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    json(response).await["nextCursor"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn csrf_token(app: &axum::Router) -> String {
    let response = app
        .clone()
        .oneshot(request("/api/v1/session", true))
        .await
        .unwrap();
    json(response).await["csrf_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn lists_stably_with_cursor_windows_ordering_and_project_relative_paths() {
    let (app, _directory, pool, project_id) = fixture().await;
    let ids = [FileEntryId::new(), FileEntryId::new(), FileEntryId::new()];
    for (id, name) in ids.into_iter().zip(["a.txt", "A2.txt", "b.txt"]) {
        insert_entry(&pool, id, project_id, None, name, "file", "live").await;
    }

    let first = app
        .clone()
        .oneshot(request(
            &format!("/api/v1/projects/{project_id}/files?limit=2"),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first = json(first).await;
    assert_eq!(first["items"].as_array().unwrap().len(), 2);
    assert_eq!(first["items"][0]["exactName"], "a.txt");
    assert_eq!(first["items"][0]["relativePath"], "a.txt");
    assert!(first["snapshotVersion"].as_str().is_some());
    let cursor = first["nextCursor"].as_str().unwrap();

    let second = app
        .oneshot(request(
            &format!("/api/v1/projects/{project_id}/files?limit=2&cursor={cursor}"),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let second = json(second).await;
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert_eq!(second["items"][0]["exactName"], "b.txt");
    assert!(second["nextCursor"].is_null());
    pool.close().await;
}

#[tokio::test]
async fn lists_root_and_child_visible_states_without_internal_host_identity() {
    let (app, _directory, pool, project_id) = fixture().await;
    let folder = FileEntryId::new();
    insert_entry(
        &pool,
        folder,
        project_id,
        None,
        "folder",
        "directory",
        "live",
    )
    .await;
    for (state, name) in [
        ("live", "live.txt"),
        ("settling", "settling.txt"),
        ("unsupported", "unsupported.txt"),
        ("missing", "missing.txt"),
        ("trashed", "trashed.txt"),
    ] {
        insert_entry(
            &pool,
            FileEntryId::new(),
            project_id,
            Some(folder),
            name,
            "file",
            state,
        )
        .await;
    }
    let response = app
        .oneshot(request(
            &format!("/api/v1/projects/{project_id}/files?parentId={folder}"),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 3);
    for item in body["items"].as_array().unwrap() {
        assert!(
            item["relativePath"]
                .as_str()
                .unwrap()
                .starts_with("folder/")
        );
        assert!(item["size"].is_string());
        assert!(item["revision"].is_string());
        assert!(item.get("platformIdentity").is_none());
        assert!(item.get("platformKind").is_none());
        assert!(!item.to_string().contains(":\\"));
    }
    pool.close().await;
}

#[tokio::test]
async fn any_catalog_insert_rename_trash_or_content_change_stales_the_next_page() {
    let (app, _directory, pool, project_id) = fixture().await;
    let first = FileEntryId::new();
    let second = FileEntryId::new();
    insert_entry(&pool, first, project_id, None, "a", "file", "live").await;
    insert_entry(&pool, second, project_id, None, "b", "file", "live").await;

    let cursor = first_cursor(&app, project_id).await;
    insert_entry(
        &pool,
        FileEntryId::new(),
        project_id,
        None,
        "c",
        "file",
        "live",
    )
    .await;
    assert_file_error(
        app.clone()
            .oneshot(request(
                &format!("/api/v1/projects/{project_id}/files?limit=1&cursor={cursor}"),
                true,
            ))
            .await
            .unwrap(),
        StatusCode::CONFLICT,
        "stale_file_snapshot",
    )
    .await;

    for statement in [
        "UPDATE file_entry SET exact_name = 'renamed' WHERE exact_name = 'b'",
        "UPDATE file_entry SET state = 'trashed' WHERE exact_name = 'renamed'",
        "UPDATE file_entry SET size = size + 1 WHERE exact_name = 'a'",
    ] {
        let cursor = first_cursor(&app, project_id).await;
        sqlx::query(statement).execute(&pool).await.unwrap();
        assert_file_error(
            app.clone()
                .oneshot(request(
                    &format!("/api/v1/projects/{project_id}/files?limit=1&cursor={cursor}"),
                    true,
                ))
                .await
                .unwrap(),
            StatusCode::CONFLICT,
            "stale_file_snapshot",
        )
        .await;
    }

    let folder = FileEntryId::new();
    insert_entry(
        &pool,
        folder,
        project_id,
        None,
        "folder",
        "directory",
        "live",
    )
    .await;
    for name in ["child-a", "child-b"] {
        insert_entry(
            &pool,
            FileEntryId::new(),
            project_id,
            Some(folder),
            name,
            "file",
            "live",
        )
        .await;
    }
    let response = app
        .clone()
        .oneshot(request(
            &format!("/api/v1/projects/{project_id}/files?parentId={folder}&limit=1"),
            true,
        ))
        .await
        .unwrap();
    let cursor = json(response).await["nextCursor"]
        .as_str()
        .unwrap()
        .to_owned();
    sqlx::query("UPDATE file_entry SET state = 'trashed' WHERE id = ?")
        .bind(folder.to_string())
        .execute(&pool)
        .await
        .unwrap();
    assert_file_error(
        app.clone()
            .oneshot(request(
                &format!(
                    "/api/v1/projects/{project_id}/files?parentId={folder}&limit=1&cursor={cursor}"
                ),
                true,
            ))
            .await
            .unwrap(),
        StatusCode::CONFLICT,
        "stale_file_snapshot",
    )
    .await;
    pool.close().await;
}

#[tokio::test]
async fn cursor_is_canonical_bounded_and_bound_to_project_and_parent() {
    let (app, _directory, pool, project_id) = fixture().await;
    insert_entry(
        &pool,
        FileEntryId::new(),
        project_id,
        None,
        "a",
        "file",
        "live",
    )
    .await;
    insert_entry(
        &pool,
        FileEntryId::new(),
        project_id,
        None,
        "b",
        "file",
        "live",
    )
    .await;
    let cursor = first_cursor(&app, project_id).await;
    let other = ProjectId::new();
    insert_project(&pool, other, "active").await;
    let folder = FileEntryId::new();
    insert_entry(
        &pool,
        folder,
        project_id,
        None,
        "folder",
        "directory",
        "live",
    )
    .await;

    for uri in [
        format!("/api/v1/projects/{other}/files?limit=1&cursor={cursor}"),
        format!("/api/v1/projects/{project_id}/files?parentId={folder}&limit=1&cursor={cursor}"),
    ] {
        assert_file_error(
            app.clone().oneshot(request(&uri, true)).await.unwrap(),
            StatusCode::BAD_REQUEST,
            "invalid_file_cursor",
        )
        .await;
    }

    let bad_envelopes = [
        json!({"version":1,"projectId":project_id.to_string(),"parentId":null,
               "snapshotVersion":"01","lastExactName":"a","lastEntryId":FileEntryId::new().to_string()}),
        json!({"version":2,"projectId":project_id.to_string(),"parentId":null,
               "snapshotVersion":"1","lastExactName":"a","lastEntryId":FileEntryId::new().to_string()}),
        json!({"version":1,"projectId":project_id.to_string(),"parentId":null,
               "snapshotVersion":"1","lastExactName":"a","lastEntryId":"not-a-uuid"}),
        json!({"version":1,"projectId":project_id.to_string().to_uppercase(),"parentId":null,
               "snapshotVersion":"1","lastExactName":"a","lastEntryId":FileEntryId::new().to_string()}),
        json!({"version":1,"projectId":project_id.to_string(),"parentId":null,
               "snapshotVersion":"1","lastExactName":"a",
               "lastEntryId":FileEntryId::new().to_string().replace('-', "")}),
        json!({"version":1,"projectId":project_id.to_string(),"parentId":null,
               "snapshotVersion":"1","lastExactName":"a","lastEntryId":FileEntryId::new().to_string(),"unknown":true}),
    ];
    let decoded = URL_SAFE_NO_PAD.decode(&cursor).unwrap();
    let mut spaced = Vec::with_capacity(decoded.len() + 2);
    spaced.push(b' ');
    spaced.extend_from_slice(&decoded);
    spaced.push(b' ');
    let mut invalid = vec![
        "garbage".to_owned(),
        format!("{cursor}="),
        URL_SAFE_NO_PAD.encode(spaced),
    ];
    invalid.extend(
        bad_envelopes
            .into_iter()
            .map(|value| URL_SAFE_NO_PAD.encode(value.to_string())),
    );
    invalid.push("A".repeat(MAX_FILE_CURSOR_BYTES + 1));
    invalid.push(
        URL_SAFE_NO_PAD.encode(
            json!({"version":1,"projectId":project_id.to_string(),"parentId":null,
               "snapshotVersion":"1","lastExactName":"x".repeat(1021),
               "lastEntryId":FileEntryId::new().to_string()})
            .to_string(),
        ),
    );
    for cursor in invalid {
        assert_file_error(
            app.clone()
                .oneshot(request(
                    &format!("/api/v1/projects/{project_id}/files?cursor={cursor}"),
                    true,
                ))
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            "invalid_file_cursor",
        )
        .await;
    }
    pool.close().await;
}

#[tokio::test]
async fn query_limits_duplicates_and_total_length_are_rejected_stably() {
    let (app, _directory, pool, project_id) = fixture().await;
    let duplicate_parent = FileEntryId::new();
    for (query, code) in [
        ("limit=0".to_owned(), "invalid_file_limit"),
        ("limit=01".to_owned(), "invalid_file_limit"),
        ("limit=501".to_owned(), "invalid_file_limit"),
        ("limit=1&limit=2".to_owned(), "invalid_file_query"),
        ("parentId=x".to_owned(), "invalid_parent_id"),
        (
            format!("parentId={duplicate_parent}&parentId={duplicate_parent}"),
            "invalid_file_query",
        ),
        ("unknown=x".to_owned(), "invalid_file_query"),
    ] {
        assert_file_error(
            app.clone()
                .oneshot(request(
                    &format!("/api/v1/projects/{project_id}/files?{query}"),
                    true,
                ))
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            code,
        )
        .await;
    }
    let oversized = format!("unknown={}", "x".repeat(MAX_FILE_QUERY_BYTES));
    assert_file_error(
        app.clone()
            .oneshot(request(
                &format!("/api/v1/projects/{project_id}/files?{oversized}"),
                true,
            ))
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        "invalid_file_query",
    )
    .await;
    assert_eq!(
        app.clone()
            .oneshot(request(
                &format!("/api/v1/projects/{project_id}/files?limit=500"),
                true,
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    pool.close().await;
}

#[tokio::test]
async fn default_and_max_page_sizes_have_exact_limit_plus_one_boundaries() {
    let (app, _directory, pool, project_id) = fixture().await;
    for index in 0..101 {
        insert_entry(
            &pool,
            FileEntryId::new(),
            project_id,
            None,
            &format!("item-{index:03}"),
            "file",
            "live",
        )
        .await;
    }
    let response = app
        .clone()
        .oneshot(request(
            &format!("/api/v1/projects/{project_id}/files"),
            true,
        ))
        .await
        .unwrap();
    let body = json(response).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 100);
    assert!(body["nextCursor"].is_string());

    let exact = app
        .oneshot(request(
            &format!("/api/v1/projects/{project_id}/files?limit=101"),
            true,
        ))
        .await
        .unwrap();
    let exact = json(exact).await;
    assert_eq!(exact["items"].as_array().unwrap().len(), 101);
    assert!(exact["nextCursor"].is_null());
    pool.close().await;
}

#[tokio::test]
async fn project_folder_and_auth_boundaries_use_stable_envelopes() {
    let (app, _directory, pool, project_id) = fixture().await;
    let missing_project = ProjectId::new();
    let missing_folder = FileEntryId::new();
    let file = FileEntryId::new();
    let unsupported = FileEntryId::new();
    insert_entry(&pool, file, project_id, None, "file", "file", "live").await;
    insert_entry(
        &pool,
        unsupported,
        project_id,
        None,
        "unsupported",
        "directory",
        "unsupported",
    )
    .await;

    assert_eq!(
        app.clone()
            .oneshot(request(
                &format!("/api/v1/projects/{project_id}/files"),
                false,
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let mut wrong_subject = request(&format!("/api/v1/projects/{project_id}/files"), false);
    let mut wrong_claims = claims();
    wrong_claims.sub = "other-owner".into();
    wrong_subject.extensions_mut().insert(wrong_claims);
    assert_eq!(
        app.clone().oneshot(wrong_subject).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    for (uri, status, code) in [
        (
            format!("/api/v1/projects/{missing_project}/files"),
            StatusCode::NOT_FOUND,
            "project_not_found",
        ),
        (
            format!("/api/v1/projects/{project_id}/files?parentId={missing_folder}"),
            StatusCode::NOT_FOUND,
            "folder_not_found",
        ),
        (
            format!("/api/v1/projects/{project_id}/files?parentId={file}"),
            StatusCode::CONFLICT,
            "not_a_folder",
        ),
        (
            format!("/api/v1/projects/{project_id}/files?parentId={unsupported}"),
            StatusCode::CONFLICT,
            "unsupported_file_entry",
        ),
    ] {
        assert_file_error(
            app.clone().oneshot(request(&uri, true)).await.unwrap(),
            status,
            code,
        )
        .await;
    }

    let mut unsafe_method = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/projects/{project_id}/files"))
        .body(Body::empty())
        .unwrap();
    unsafe_method.extensions_mut().insert(claims());
    assert_eq!(
        app.clone().oneshot(unsafe_method).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let token = csrf_token(&app).await;
    let mut authorized_method = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/projects/{project_id}/files"))
        .body(Body::empty())
        .unwrap();
    authorized_method.extensions_mut().insert(claims());
    authorized_method
        .headers_mut()
        .insert(header::ORIGIN, ORIGIN.parse().unwrap());
    authorized_method.headers_mut().insert(
        HeaderName::from_static("x-cellar-csrf"),
        token.parse().unwrap(),
    );
    let response = app.oneshot(authorized_method).await.unwrap();
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );
    assert_file_error(
        response,
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
    )
    .await;
    pool.close().await;
}

#[tokio::test]
async fn malformed_paths_and_request_ids_fail_before_catalog_access() {
    let (app, _directory, pool, project_id) = fixture().await;
    for uri in [
        "/api/v1/projects/not-a-uuid/files".to_owned(),
        format!("/api/v1/projects/{project_id}/files/"),
        format!("/api/v1/projects/{project_id}/files/extra"),
    ] {
        assert_file_error(
            app.clone().oneshot(request(&uri, true)).await.unwrap(),
            StatusCode::BAD_REQUEST,
            if uri.ends_with("/files") {
                "invalid_project_id"
            } else {
                "invalid_file_path"
            },
        )
        .await;
    }
    let mut duplicate = request(&format!("/api/v1/projects/{project_id}/files"), true);
    let request_id = HeaderName::from_static("x-request-id");
    duplicate
        .headers_mut()
        .append(&request_id, "one".parse().unwrap());
    duplicate
        .headers_mut()
        .append(&request_id, "two".parse().unwrap());
    assert_file_error(
        app.oneshot(duplicate).await.unwrap(),
        StatusCode::BAD_REQUEST,
        "invalid_request_id",
    )
    .await;
    pool.close().await;
}

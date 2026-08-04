mod common;

use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    Router,
    body::{Body, to_bytes},
};
use cellar::{
    app::secure_cellar_api_router, config::Config, db::Database, storage::Storage,
    uploads::UploadService,
};
use http::{StatusCode, header};
use serde_json::json;
use tower::ServiceExt;
use uuid::Uuid;

use common::{
    EXTERNAL_ORIGIN, FakeAccessVerifier, TestContext, authenticated_request,
    authenticated_write_request, json_request, json_response,
};

const MIB: usize = 1024 * 1024;
const CHUNK_SIZE: usize = 32 * MIB;
const TOTAL_SIZE: usize = 96 * MIB;
const FILE_NAME: &str = "acceptance-96-mib.bin";
const CHUNK_MARKERS: [u8; 3] = [0x11, 0xa5, 0x5c];

struct RestartedApp {
    app: Router,
    database: Database,
    storage: Arc<Storage>,
}

#[tokio::test]
async fn local_upload_survives_restart_and_downloads_exact_bytes() {
    let context = TestContext::new().await;
    let app = context.app();
    let project_id = create_project(&app, "Local acceptance").await;
    let upload_id = create_upload(&app, project_id, FILE_NAME, TOTAL_SIZE).await;

    for (index, marker) in CHUNK_MARKERS.into_iter().enumerate() {
        let offset = index * CHUNK_SIZE;
        let response = app
            .clone()
            .oneshot(chunk_request(upload_id, offset, vec![marker; CHUNK_SIZE]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response.headers()["upload-offset"],
            ((index + 1) * CHUNK_SIZE).to_string()
        );
    }
    drop(app);

    let restarted = restart(&context).await;
    let (status_code, _, status) = json_response(
        restarted
            .app
            .clone()
            .oneshot(
                authenticated_request("GET", &upload_path(upload_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status_code, StatusCode::OK);
    assert_eq!(status["committedOffset"], TOTAL_SIZE.to_string());
    assert_eq!(status["state"], "active");

    let (complete_status, _, completed) = json_response(
        restarted
            .app
            .clone()
            .oneshot(
                authenticated_write_request(&complete_path(upload_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(complete_status, StatusCode::OK);
    assert_eq!(completed["committedOffset"], TOTAL_SIZE.to_string());
    assert_eq!(completed["state"], "complete");

    let (list_status, _, files) = json_response(
        restarted
            .app
            .clone()
            .oneshot(
                authenticated_request("GET", &files_path(project_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(list_status, StatusCode::OK);
    assert_eq!(files.as_array().unwrap().len(), 1);
    assert_eq!(files[0]["name"], FILE_NAME);
    assert_eq!(files[0]["size"], TOTAL_SIZE.to_string());
    assert_eq!(
        std::fs::metadata(
            context
                .project_path(project_id)
                .join("files")
                .join(FILE_NAME)
        )
        .unwrap()
        .len(),
        TOTAL_SIZE as u64
    );

    let full_response = restarted
        .app
        .clone()
        .oneshot(
            authenticated_request("GET", &download_path(project_id, FILE_NAME))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(full_response.status(), StatusCode::OK);
    assert_eq!(
        full_response.headers()[header::CONTENT_LENGTH],
        TOTAL_SIZE.to_string()
    );
    let full_body = to_bytes(full_response.into_body(), TOTAL_SIZE + 1)
        .await
        .unwrap();
    assert_eq!(full_body.len(), TOTAL_SIZE);
    for (index, marker) in CHUNK_MARKERS.into_iter().enumerate() {
        let start = index * CHUNK_SIZE;
        let end = start + CHUNK_SIZE;
        assert!(
            full_body[start..end].iter().all(|byte| *byte == marker),
            "downloaded chunk {index} did not match its uploaded bytes"
        );
    }

    let range_start = CHUNK_SIZE - 16;
    let range_end = CHUNK_SIZE + 15;
    let range_response = restarted
        .app
        .clone()
        .oneshot(
            authenticated_request("GET", &download_path(project_id, FILE_NAME))
                .header(header::RANGE, format!("bytes={range_start}-{range_end}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(range_response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        range_response.headers()[header::CONTENT_RANGE],
        format!("bytes {range_start}-{range_end}/{TOTAL_SIZE}")
    );
    let range_body = to_bytes(range_response.into_body(), 33).await.unwrap();
    let expected_range = [vec![CHUNK_MARKERS[0]; 16], vec![CHUNK_MARKERS[1]; 16]].concat();
    assert_eq!(range_body.as_ref(), expected_range.as_slice());

    restarted.database.close().await;
    context.close().await;
}

#[tokio::test]
async fn startup_recovery_truncates_interrupted_upload_to_committed_offset() {
    const COMMITTED: usize = MIB;
    const TOTAL: usize = 2 * MIB;
    const TRAILING: usize = 257 * 1024;
    const MARKER: u8 = 0x6d;

    let context = TestContext::new().await;
    let app = context.app();
    let project_id = create_project(&app, "Interrupted acceptance").await;
    let upload_id = create_upload(&app, project_id, "interrupted.bin", TOTAL).await;
    let response = app
        .clone()
        .oneshot(chunk_request(upload_id, 0, vec![MARKER; COMMITTED]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    drop(app);

    let staging_path = staging_path(context.temp.path(), upload_id);
    OpenOptions::new()
        .append(true)
        .open(&staging_path)
        .unwrap()
        .write_all(&vec![0xee; TRAILING])
        .unwrap();
    assert_eq!(
        std::fs::metadata(&staging_path).unwrap().len(),
        (COMMITTED + TRAILING) as u64
    );

    let restarted = restart(&context).await;
    assert_eq!(
        restarted.storage.staging_len(upload_id).await.unwrap(),
        Some(COMMITTED as u64)
    );
    let repaired = std::fs::read(&staging_path).unwrap();
    assert_eq!(repaired.len(), COMMITTED);
    assert!(repaired.iter().all(|byte| *byte == MARKER));

    let (status_code, _, status) = json_response(
        restarted
            .app
            .clone()
            .oneshot(
                authenticated_request("GET", &upload_path(upload_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status_code, StatusCode::OK);
    assert_eq!(status["committedOffset"], COMMITTED.to_string());
    assert_eq!(status["state"], "active");

    restarted.database.close().await;
    context.close().await;
}

async fn create_project(app: &Router, name: &str) -> Uuid {
    let (status, _, project) = json_response(
        app.clone()
            .oneshot(json_request(
                authenticated_write_request("/api/v1/projects"),
                &json!({"name": name}).to_string(),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    Uuid::parse_str(project["id"].as_str().unwrap()).unwrap()
}

async fn create_upload(app: &Router, project_id: Uuid, file_name: &str, total_size: usize) -> Uuid {
    let (status, _, upload) = json_response(
        app.clone()
            .oneshot(json_request(
                authenticated_write_request(&uploads_path(project_id)),
                &json!({
                    "fileName": file_name,
                    "totalSize": total_size.to_string(),
                })
                .to_string(),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(upload["committedOffset"], "0");
    Uuid::parse_str(upload["id"].as_str().unwrap()).unwrap()
}

fn chunk_request(upload_id: Uuid, offset: usize, bytes: Vec<u8>) -> http::Request<Body> {
    let length = bytes.len();
    authenticated_request("PUT", &chunk_path(upload_id))
        .header("origin", EXTERNAL_ORIGIN)
        .header("content-type", "application/octet-stream")
        .header("upload-offset", offset.to_string())
        .header(header::CONTENT_LENGTH, length.to_string())
        .body(Body::from(bytes))
        .unwrap()
}

async fn restart(context: &TestContext) -> RestartedApp {
    context.database.clone().close().await;
    let root = context.temp.path().canonicalize().unwrap();
    let storage = Arc::new(Storage::new(root.clone()).unwrap());
    storage.initialize().await.unwrap();
    let database = Database::open(root.join(".cellar").join("cellar.db"))
        .await
        .unwrap();
    UploadService::new(Arc::new(database.clone()), storage.clone())
        .recover_uploads()
        .await
        .unwrap();
    let config = config_for(&root);
    let app = secure_cellar_api_router(
        Arc::new(database.clone()),
        storage.clone(),
        Arc::new(FakeAccessVerifier),
        config.external_origin(),
    );
    RestartedApp {
        app,
        database,
        storage,
    }
}

fn config_for(root: &Path) -> Config {
    let database_path = root.join(".cellar").join("cellar.db");
    Config::parse(&format!(
        r#"bind = "127.0.0.1:8787"
external_origin = "{EXTERNAL_ORIGIN}"
data_root = "{}"
database_path = "{}"

[access]
team_domain = "https://example.cloudflareaccess.com"
audience = "test-audience"
owner_email = "owner@example.com"
"#,
        toml_path(root),
        toml_path(&database_path),
    ))
    .unwrap()
}

fn toml_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn staging_path(root: &Path, upload_id: Uuid) -> PathBuf {
    root.join(".cellar")
        .join("uploads")
        .join(format!("{upload_id}.part"))
}

fn uploads_path(project_id: Uuid) -> String {
    format!("/api/v1/projects/{project_id}/uploads")
}

fn upload_path(upload_id: Uuid) -> String {
    format!("/api/v1/uploads/{upload_id}")
}

fn chunk_path(upload_id: Uuid) -> String {
    format!("/api/v1/uploads/{upload_id}/chunk")
}

fn complete_path(upload_id: Uuid) -> String {
    format!("/api/v1/uploads/{upload_id}/complete")
}

fn files_path(project_id: Uuid) -> String {
    format!("/api/v1/projects/{project_id}/files")
}

fn download_path(project_id: Uuid, file_name: &str) -> String {
    format!("/api/v1/projects/{project_id}/files/{file_name}")
}

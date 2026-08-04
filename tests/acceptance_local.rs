mod common;

use std::{path::Path, sync::Arc};

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

const FILE_NAME: &str = "acceptance.bin";

struct RestartedApp {
    app: Router,
    database: Database,
}

#[tokio::test]
async fn one_request_upload_lists_and_downloads_exact_bytes_after_restart() {
    let context = TestContext::new().await;
    let app = context.app();
    let project_id = create_project(&app, "Local acceptance").await;
    let bytes = (0..(4 * 1024 * 1024))
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();

    let (upload_status, _, uploaded) = json_response(
        app.clone()
            .oneshot(
                authenticated_write_request(&format!(
                    "/api/v1/projects/{project_id}/uploads?fileName={FILE_NAME}"
                ))
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body(Body::from(bytes.clone()))
                .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(upload_status, StatusCode::CREATED);
    assert_eq!(uploaded["name"], FILE_NAME);
    assert_eq!(uploaded["size"], bytes.len().to_string());

    let (list_status, _, files) = json_response(
        app.clone()
            .oneshot(
                authenticated_request("GET", &format!("/api/v1/projects/{project_id}/files"))
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

    drop(app);
    let restarted = restart(&context).await;
    let download = restarted
        .app
        .clone()
        .oneshot(
            authenticated_request(
                "GET",
                &format!("/api/v1/projects/{project_id}/files/{FILE_NAME}"),
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::OK);
    let downloaded = to_bytes(download.into_body(), bytes.len() + 1)
        .await
        .unwrap();
    assert_eq!(downloaded.as_ref(), bytes.as_slice());

    restarted.database.close().await;
    context.close().await;
}

#[tokio::test]
async fn restart_removes_incomplete_temporary_uploads() {
    let context = TestContext::new().await;
    let upload_id = Uuid::now_v7();
    context.storage.create_staging(upload_id).await.unwrap();
    context
        .storage
        .write_chunk(upload_id, 0, &b"partial"[..])
        .await
        .unwrap();
    assert_eq!(
        context.storage.staging_len(upload_id).await.unwrap(),
        Some(7)
    );

    let restarted = restart(&context).await;

    assert_eq!(context.storage.staging_len(upload_id).await.unwrap(), None);
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
        storage,
        Arc::new(FakeAccessVerifier),
        config.external_origin(),
    );
    RestartedApp { app, database }
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

//! Shared integration-test fixtures.

#![allow(dead_code)]

use std::{path::Path, sync::Arc};

use axum::{Router, body::Body, http::Request};
use cellar::{
    app::{X_REQUEST_ID, secure_project_api_router},
    auth::{AccessError, AccessVerifier, OwnerIdentity},
    config::Config,
    db::Database,
    projects::ProjectService,
    storage::Storage,
};
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::TempDir;

pub const ASSERTION_HEADER: &str = "Cf-Access-Jwt-Assertion";
pub const VALID_ASSERTION: &str = "fixture-valid-assertion";
pub const OWNER_EMAIL: &str = "owner@example.com";
pub const EXTERNAL_ORIGIN: &str = "https://files.example.com";

#[derive(Clone)]
pub struct FakeAccessVerifier;

impl AccessVerifier for FakeAccessVerifier {
    fn verify<'a>(
        &'a self,
        assertion: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OwnerIdentity, AccessError>> + Send + 'a>,
    > {
        Box::pin(async move {
            match assertion {
                VALID_ASSERTION => OwnerIdentity::try_from_email(" OWNER@Example.COM "),
                _ => Err(AccessError::unauthenticated()),
            }
        })
    }
}

pub struct TestContext {
    pub temp: TempDir,
    pub database: Database,
    pub storage: Arc<Storage>,
    config: Config,
}

impl TestContext {
    pub async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let storage = Arc::new(Storage::new(root.clone()).unwrap());
        storage.initialize().await.unwrap();
        let database = Database::open(root.join(".cellar").join("cellar.db"))
            .await
            .unwrap();
        let config = Config::parse(&format!(
            r#"bind = "127.0.0.1:8787"
external_origin = "{EXTERNAL_ORIGIN}"
data_root = "{}"
database_path = "{}"

[access]
team_domain = "https://example.cloudflareaccess.com"
audience = "test-audience"
owner_email = "{OWNER_EMAIL}"
"#,
            toml_path(&root),
            toml_path(&root.join(".cellar").join("cellar.db")),
        ))
        .unwrap();

        Self {
            temp,
            database,
            storage,
            config,
        }
    }

    pub fn app(&self) -> Router {
        self.secure(ProjectService::new(
            Arc::new(self.database.clone()),
            self.storage.clone(),
        ))
    }

    pub fn secure(&self, service: ProjectService) -> Router {
        secure_project_api_router(
            service,
            Arc::new(FakeAccessVerifier),
            self.config.external_origin(),
        )
    }

    pub fn project_path(&self, id: uuid::Uuid) -> std::path::PathBuf {
        self.temp.path().join("projects").join(id.to_string())
    }

    pub async fn close(self) {
        self.database.close().await;
    }
}

pub fn authenticated_request(method: &str, uri: &str) -> http::request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(ASSERTION_HEADER, VALID_ASSERTION)
}

pub fn authenticated_write_request(uri: &str) -> http::request::Builder {
    authenticated_request("POST", uri).header("origin", EXTERNAL_ORIGIN)
}

pub async fn json_response(
    response: axum::response::Response,
) -> (http::StatusCode, String, Value) {
    let status = response.status();
    let request_id = response
        .headers()
        .get(&X_REQUEST_ID)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, request_id, serde_json::from_slice(&bytes).unwrap())
}

pub fn json_request(builder: http::request::Builder, body: &str) -> Request<Body> {
    builder
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn toml_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

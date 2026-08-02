#![cfg(windows)]

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use cellar_api::health::Readiness;
use cellar_api::routes::files::DownloadSource as _;
use cellar_auth::{AccessClaims, OwnerMode};
use cellar_config::{CellarConfig, PersistedConfig, save_config};
use cellar_core::{FileEntryId, FileService, ProjectId, ReadinessBlocker};
use cellar_db::{FilenameCollation, SqliteFileRepository};
use cellar_service::app::{
    OriginAuthenticator, Shutdown, initialize_upload_recovery,
    origin_router_with_authenticator_and_services,
};
use cellar_service::downloads::{
    ProductionDownloadSource, ReconciliationRequest, ReconciliationScheduler,
    SqliteDownloadCatalog, WindowsDownloadPlatform,
};
use cellar_windows::{WindowsName, WindowsStorage, WindowsUploadStaging};
use http_body_util::BodyExt as _;
use tempfile::tempdir;
use time::OffsetDateTime;
use tower::ServiceExt as _;
use url::Url;

struct AllowOwner;

#[async_trait]
impl OriginAuthenticator for AllowOwner {
    async fn validate(
        &self,
        token: &str,
        _owner_mode: OwnerMode<'_>,
    ) -> Result<AccessClaims, cellar_auth::AuthError> {
        assert_eq!(token, "signed-access-token");
        Ok(AccessClaims {
            iss: "https://team.cloudflareaccess.com".into(),
            aud: vec!["audience".into()],
            sub: "owner-subject".into(),
            email: None,
            exp: i64::MAX,
            nbf: 0,
            iat: 0,
            r#type: "app".into(),
        })
    }

    fn max_token_len(&self) -> usize {
        1024
    }
}

struct NoopScheduler;

impl ReconciliationScheduler for NoopScheduler {
    fn try_schedule(&self, _request: ReconciliationRequest) -> bool {
        true
    }
}

fn authenticated(method: Method, path: String, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("cf-access-jwt-assertion", "signed-access-token")
        .body(body)
        .unwrap()
}

#[tokio::test]
async fn production_origin_shares_one_trusted_root_between_uploads_and_downloads() {
    let directory = tempdir().unwrap();
    let storage_root = directory.path().join("storage");
    let database_path = directory.path().join("cellar.db");
    let config_path = directory.path().join("config.toml");
    let project_id = ProjectId::new();
    let file_id = FileEntryId::new();
    let payload_path = storage_root
        .join("projects")
        .join(project_id.to_string())
        .join("files")
        .join("payload.bin");
    std::fs::create_dir_all(payload_path.parent().unwrap()).unwrap();
    std::fs::write(&payload_path, b"abcdef").unwrap();

    save_config(
        &config_path,
        &PersistedConfig {
            config: CellarConfig {
                external_origin: Url::parse("https://cellar.example.test").unwrap(),
                team_domain: Url::parse("https://team.cloudflareaccess.com").unwrap(),
                aud_tags: vec!["audience".into()],
                bootstrap_owner_email: None,
                owner_subject: Some("owner-subject".into()),
                storage_root: storage_root.clone(),
                origin_port: 8443,
                health_port: 8081,
            },
            bootstrap_claim: None,
        },
    )
    .unwrap();

    let identity = cellar_windows::preflight::open_as_service(&storage_root).unwrap();
    let storage = WindowsStorage::adopt(identity).unwrap();
    let staging = Arc::new(WindowsUploadStaging::open(storage.clone()).unwrap());

    let pool = cellar_db::open_pool(
        &database_path,
        FilenameCollation::windows_ordinal_ci_v1(str::cmp),
    )
    .await
    .unwrap();
    cellar_db::migrate(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO project
         (id, name, description, status, version, created_at, updated_at)
         VALUES (?, 'Production', '', 'active', 1,
                 '2026-08-02T00:00:00Z', '2026-08-02T00:00:00Z')",
    )
    .bind(project_id.to_string())
    .execute(&pool)
    .await
    .unwrap();

    let mut parent = storage.root().clone();
    for component in ["projects", project_id.to_string().as_str(), "files"] {
        parent = storage
            .open_verified(&parent, &WindowsName::parse(component).unwrap())
            .unwrap();
    }
    let payload = storage
        .open_download_verified(&parent, &WindowsName::parse("payload.bin").unwrap())
        .unwrap();
    let metadata = storage.download_metadata(&payload).unwrap();
    assert!(
        metadata.mtime_filetime_100ns >= 0,
        "download metadata must fit the catalog's signed FILETIME: {metadata:?}"
    );
    drop(payload);
    drop(parent);
    sqlx::query(
        "INSERT INTO file_entry
         (id, project_id, parent_id, exact_name, kind, platform_kind,
          volume_serial, filesystem_file_id, size, mtime_filetime_100ns,
          hash, hash_state, state, revision, scan_generation, observed_at)
         VALUES (?, ?, NULL, 'payload.bin', 'file', 'windows_file_id', ?, ?,
                 ?, ?, NULL, 'computing', 'live', 1, 1,
                 '2026-08-02T00:00:00Z')",
    )
    .bind(file_id.to_string())
    .bind(project_id.to_string())
    .bind(metadata.identity.volume_serial.to_le_bytes().as_slice())
    .bind(metadata.identity.file_id.to_le_bytes().as_slice())
    .bind(i64::try_from(metadata.length).unwrap())
    .bind(metadata.mtime_filetime_100ns)
    .execute(&pool)
    .await
    .unwrap();

    let readiness = Readiness::new([ReadinessBlocker::RecoveryRequired]);
    let upload_service =
        initialize_upload_recovery(&pool, staging, &readiness, OffsetDateTime::now_utc())
            .await
            .unwrap();
    assert!(!readiness.blocker_codes().contains(&"recovery_required"));

    let file_service = FileService::new(Arc::new(SqliteFileRepository::new(pool.clone())));
    let download_source = Arc::new(ProductionDownloadSource::new(
        Arc::new(SqliteDownloadCatalog::new(pool.clone())),
        Arc::new(WindowsDownloadPlatform::new(storage)),
        Arc::new(NoopScheduler),
    ));
    let direct = download_source
        .open_verified(project_id, file_id)
        .await
        .expect("production download source opens from the shared trusted root");
    direct
        .verify()
        .await
        .expect("catalog identity matches the shared trusted root handle");
    let app = origin_router_with_authenticator_and_services(
        &config_path,
        Arc::new(AllowOwner),
        readiness,
        Shutdown::new(),
        upload_service,
        file_service,
        download_source,
    );

    let download = app
        .clone()
        .oneshot(authenticated(
            Method::GET,
            format!("/api/v1/projects/{project_id}/files/{file_id}/download"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::OK);
    assert_eq!(
        download.into_body().collect().await.unwrap().to_bytes(),
        b"abcdef".as_slice()
    );

    let upload = app
        .oneshot(authenticated(
            Method::POST,
            "/api/v1/uploads".into(),
            Body::from("{}"),
        ))
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::FORBIDDEN);

    pool.close().await;
}
